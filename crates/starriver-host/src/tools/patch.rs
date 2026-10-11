//! Patch tool: apply unified diffs with unique context matching (line
//! numbers are hints; the context block must match exactly once), either a
//! single file (`path` + `diff`) or a multi-file envelope (`files:
//! [{path, diff}]`).
//!
//! Semantics notes:
//! - `@@ -o,c +n,c @@` header positions are parsed but only used as hints;
//!   `new_start`/`new_count` are not used for matching.
//! - A `@@ -0,0` hunk on an *absent* file marks creation; on an *existing*
//!   file it is a legal header insertion (prepend) and applies normally.
//! - Per-file application is all-or-nothing: every hunk is matched before the
//!   file is written, and the write goes through a same-directory temp file
//!   plus rename so a crash cannot leave a partial file.
//! - The multi-file envelope is two-phase: every entry is staged (resolved,
//!   sandbox-checked, hunk-matched in memory) before anything is written; a
//!   write failure mid-way rolls back the already-written entries (modified
//!   files restored, created files removed — best-effort, reported).

use std::collections::HashSet;
use std::sync::Arc;

use cordis_rslite::Value;
use starriver_kernel::sandbox::SandboxMode;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{
    append_diff, atomic_write, invalid_input, json_args, resolve_path, text_content, tool_failure,
};

/// One `@@ -o,c +n,c @@` hunk with its body lines (' ' context, '-' removed,
/// '+' added).
struct Hunk {
    old_start: usize,
    old_count: usize,
    lines: Vec<(char, String)>,
}

fn parse_side(s: &str) -> Result<(usize, usize), String> {
    let s = s.trim_start_matches(['-', '+']);
    if s.is_empty() {
        return Ok((0, 0));
    }
    if let Some(comma) = s.find(',') {
        let start = s[..comma]
            .parse::<usize>()
            .map_err(|_| format!("bad hunk position: {s}"))?;
        let count = s[comma + 1..]
            .parse::<usize>()
            .map_err(|_| format!("bad hunk count: {s}"))?;
        Ok((start, count))
    } else {
        Ok((
            s.parse::<usize>()
                .map_err(|_| format!("bad hunk position: {s}"))?,
            1,
        ))
    }
}

fn parse_patch(diff: &str) -> Result<Vec<Hunk>, String> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;
    for raw in diff.lines() {
        let line = raw.trim_end_matches('\r');
        if line.starts_with("@@ ") {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            let rest = line.strip_prefix("@@ ").unwrap();
            let end = rest
                .find("@@")
                .ok_or_else(|| "malformed hunk header".to_string())?;
            let spec = &rest[..end];
            let parts: Vec<&str> = spec.split_whitespace().collect();
            let (old_start, old_count) = parse_side(parts.first().copied().unwrap_or(""))?;
            let (new_start, new_count) = parse_side(parts.get(1).copied().unwrap_or(""))?;
            current = Some(Hunk {
                old_start,
                old_count,
                lines: Vec::new(),
            });
            let _ = (new_start, new_count);
        } else if let Some(ref mut h) = current {
            if line.starts_with('\\')
                || line.starts_with("diff ")
                || line.starts_with("--- ")
                || line.starts_with("+++ ")
            {
                continue;
            }
            if line.is_empty() {
                h.lines.push((' ', String::new()));
            } else if line.starts_with('+') {
                h.lines
                    .push(('+', line.strip_prefix('+').unwrap().to_string()));
            } else if line.starts_with('-') {
                h.lines
                    .push(('-', line.strip_prefix('-').unwrap().to_string()));
            } else {
                // A context line is exactly ONE marker space + the original
                // text (which may itself be indented) — stripping ALL leading
                // spaces made every indented-context diff unappliable.
                h.lines.push((' ', line.strip_prefix(' ').unwrap_or(line).to_string()));
            }
        }
    }
    if let Some(h) = current {
        hunks.push(h);
    }
    Ok(hunks)
}

#[derive(Debug)]
enum PatchError {
    Mismatch(String),
    Ambiguous(String),
}

/// Positions where `block` appears verbatim in `lines`.
fn match_positions(lines: &[String], block: &[String]) -> Vec<usize> {
    if block.is_empty() {
        return Vec::new();
    }
    let max = lines.len().saturating_sub(block.len());
    let mut out = Vec::new();
    for i in 0..=max {
        if lines[i] == block[0]
            && block
                .iter()
                .enumerate()
                .all(|(j, l)| lines.get(i + j).is_some_and(|x| x == l))
        {
            out.push(i);
        }
    }
    out
}

/// Apply hunks to `base` in reverse order. Empty old-blocks (insertions)
/// anchor at `old_start` (1-based) clamped to the line count.
fn apply_patch(base: &str, diff: &str) -> Result<String, PatchError> {
    let hunks = parse_patch(diff).map_err(PatchError::Mismatch)?;
    if hunks.is_empty() {
        return Err(PatchError::Mismatch(
            "no hunks found in diff — provide a unified diff with @@ hunks".into(),
        ));
    }
    let mut lines: Vec<String> = base.lines().map(|l| l.to_string()).collect();
    for hunk in hunks.iter().rev() {
        let old_block: Vec<String> = hunk
            .lines
            .iter()
            .filter(|(k, _)| *k != '+')
            .map(|(_, s)| s.clone())
            .collect();
        let new_block: Vec<String> = hunk
            .lines
            .iter()
            .filter(|(k, _)| *k != '-')
            .map(|(_, s)| s.clone())
            .collect();
        let pos = if old_block.is_empty() {
            hunk.old_start.saturating_sub(1).min(lines.len())
        } else {
            let hits = match_positions(&lines, &old_block);
            match hits.len() {
                1 => hits[0],
                0 => {
                    let ctx: Vec<String> = old_block.iter().take(3).cloned().collect();
                    return Err(PatchError::Mismatch(format!(
                        "hunk @@ -{},{} does not match the file; expected context:\n{}",
                        hunk.old_start,
                        hunk.old_count,
                        ctx.join("\n")
                    )));
                }
                _ => {
                    let lines_at: Vec<String> = hits.iter().map(|i| (i + 1).to_string()).collect();
                    return Err(PatchError::Ambiguous(format!(
                        "hunk @@ -{},{} is ambiguous ({} occurrences at lines {}); add more context",
                        hunk.old_start,
                        hunk.old_count,
                        hits.len(),
                        lines_at.join(", ")
                    )));
                }
            }
        };
        lines.splice(pos..pos + old_block.len(), new_block);
    }
    let mut result = lines.join("\n");
    // Preserve the base file's trailing newline (real unified diffs do not
    // strip it).
    if base.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

/// One staged file: everything about the change is resolved and validated in
/// memory before any disk write happens.
struct Staged {
    path: std::path::PathBuf,
    /// `None` when the entry creates a new file (rollback deletes it).
    base: Option<String>,
    updated: String,
}

/// Parse arguments into (raw_path, diff) entries: either the single-file
/// form (`path` + `diff`) or the multi-file envelope (`files`).
fn collect_entries(json: &serde_json::Value) -> Result<Vec<(String, String)>, ToolError> {
    let has_files = json.get("files").is_some();
    let has_single = json.get("path").is_some() || json.get("diff").is_some();
    if has_files && has_single {
        return Err(invalid_input(
            "patch",
            "pass either `files` (multi-file envelope) or `path`+`diff` (single file), not both"
                .into(),
        ));
    }
    if has_files {
        let files = json
            .get("files")
            .and_then(|f| f.as_array())
            .ok_or_else(|| {
                invalid_input("patch", "files must be an array of {path, diff}".into())
            })?;
        if files.is_empty() {
            return Err(invalid_input("patch", "files must not be empty".into()));
        }
        let mut seen = HashSet::new();
        let mut out = Vec::with_capacity(files.len());
        for entry in files {
            let raw = entry
                .get("path")
                .and_then(|p| p.as_str())
                .ok_or_else(|| invalid_input("patch", "a files entry is missing path".into()))?;
            let diff = entry
                .get("diff")
                .and_then(|d| d.as_str())
                .ok_or_else(|| invalid_input("patch", "a files entry is missing diff".into()))?;
            if diff.trim().is_empty() {
                return Err(invalid_input(
                    "patch",
                    format!("diff must be non-empty for {raw}"),
                ));
            }
            if !seen.insert(raw.to_string()) {
                return Err(invalid_input(
                    "patch",
                    format!("duplicate path in files envelope: {raw}"),
                ));
            }
            out.push((raw.to_string(), diff.to_string()));
        }
        return Ok(out);
    }
    let raw = json
        .get("path")
        .and_then(|p| p.as_str())
        .ok_or_else(|| invalid_input("patch", "missing path".into()))?;
    let diff = json
        .get("diff")
        .and_then(|d| d.as_str())
        .ok_or_else(|| invalid_input("patch", "missing diff".into()))?;
    if diff.trim().is_empty() {
        return Err(invalid_input("patch", "diff must be non-empty".into()));
    }
    Ok(vec![(raw.to_string(), diff.to_string())])
}

/// Phase 1 for one entry: resolve, sandbox-check, read the base, and apply
/// the diff in memory. Nothing is written.
async fn stage_one(ctx: &ToolRunContext, raw: &str, diff: &str) -> Result<Staged, ToolError> {
    let path = resolve_path(ctx.execution.cwd.as_deref(), raw);
    crate::sandbox::ensure_workspace_writable(
        &path,
        ctx.execution.sandbox_mode,
        ctx.execution.cwd.as_deref(),
    )
    .map_err(|e| tool_failure("patch", "OUTSIDE_WORKSPACE", e.to_string()))?;
    let hunks = parse_patch(diff).map_err(|e| invalid_input("patch", e))?;
    let read = tokio::fs::read_to_string(&path).await;
    let (base, creating) = match read {
        Ok(s) => (s, false),
        Err(_) => {
            // An absent file is only a creation attempt when every hunk
            // starts at line 0 (the unified-diff creation form).
            let creating = !hunks.is_empty() && hunks.iter().all(|h| h.old_start == 0);
            if !creating {
                return Err(tool_failure(
                    "patch",
                    "NOT_FOUND",
                    format!("file not found: {}", path.display()),
                ));
            }
            if let Some(parent) = path.parent() {
                if !parent.is_dir() {
                    return Err(tool_failure(
                        "patch",
                        "NOT_FOUND",
                        format!("directory not found: {}", parent.display()),
                    ));
                }
            }
            (String::new(), true)
        }
    };
    let updated = apply_patch(&base, diff).map_err(|e| match e {
        PatchError::Mismatch(m) => tool_failure("patch", "PATCH_MISMATCH", m),
        PatchError::Ambiguous(m) => tool_failure("patch", "AMBIGUOUS_PATCH", m),
    })?;
    Ok(Staged {
        path,
        base: (!creating).then_some(base),
        updated,
    })
}

struct Patch;

impl ToolExecutor for Patch {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        Box::pin(async move {
            if ctx.execution.sandbox_mode == Some(SandboxMode::ReadOnly) {
                return Err(tool_failure(
                    "patch",
                    "READONLY_MODE",
                    "the session is in read-only sandbox mode; switch to workspace-write to modify files"
                        .into(),
                ));
            }
            let json = json_args(&arguments)?;
            let entries = collect_entries(&json)?;
            // Phase 1: stage everything. Any failure here returns before a
            // single byte reaches the disk.
            let mut staged = Vec::with_capacity(entries.len());
            for (raw, diff) in &entries {
                staged.push(stage_one(&ctx, raw, diff).await?);
            }
            // Phase 2: apply, rolling back the already-written entries when a
            // write fails mid-way.
            let mut applied: Vec<&Staged> = Vec::new();
            for item in &staged {
                if let Err(e) = atomic_write(&item.path, &item.updated).await {
                    let mut message = format!("write failed for {}: {e}", item.path.display());
                    let mut rollback_failures = Vec::new();
                    for done in applied.iter().rev() {
                        let restored = match &done.base {
                            Some(base) => atomic_write(&done.path, base).await.is_ok(),
                            None => tokio::fs::remove_file(&done.path).await.is_ok(),
                        };
                        if !restored {
                            rollback_failures.push(done.path.display().to_string());
                        }
                    }
                    message.push_str("; rolled back already-applied files");
                    if !rollback_failures.is_empty() {
                        message.push_str(&format!(
                            " (rollback FAILED for: {})",
                            rollback_failures.join(", ")
                        ));
                    }
                    return Err(tool_failure("patch", "WRITE_FAILED", message));
                }
                applied.push(item);
            }
            let mut output = if staged.len() == 1 {
                format!("patched {}", staged[0].path.display())
            } else {
                let names: Vec<String> = staged
                    .iter()
                    .map(|s| s.path.display().to_string())
                    .collect();
                format!("patched {} files: {}", staged.len(), names.join(", "))
            };
            for item in &staged {
                let path_str = item.path.display().to_string();
                let base = item.base.as_deref().unwrap_or("");
                append_diff(&mut output, &path_str, base, &item.updated);
            }
            Ok(Value::new(serde_json::json!({ "output": output })))
        })
    }
}

pub fn patch_definition() -> ToolDefinition {
    ToolDefinition {
        name: "patch".into(),
        description: "Apply unified diffs: either one file (path + diff) or several atomically (files: [{path, diff}]; every file is pre-checked and staged, and a mid-way failure rolls back what was already written). Creates a file when its hunks start at @@ -0,0. Context blocks must match exactly once.".into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "target file path, relative to cwd (single-file form)" },
                "diff": { "type": "string", "description": "unified diff, e.g. @@ -1 +1 @@\\n-old\\n+new (single-file form)" },
                "files": {
                    "type": "array",
                    "description": "multi-file envelope; each entry is {path, diff}; all-or-nothing",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "diff": { "type": "string" }
                        },
                        "required": ["path", "diff"]
                    }
                }
            },
            "required": []
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(Patch),
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
            name: "patch".into(),
            arguments: Value::new(args.clone()),
            agent_id: None,
            cwd: Some(cwd),
            sandbox_mode,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(execution);
        patch_definition()
            .executor
            .execute(Value::new(args), ctx)
            .await
    }

    #[test]
    fn parse_patch_multi_hunk() {
        let hunks = parse_patch("@@ -1,2 +1,2 @@\n-a\n-b\n+c\n+d\n@@ -5 +5 @@\n-x\n+y\n").unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].old_count, 2);
        assert_eq!(hunks[1].old_start, 5);
    }

    #[test]
    fn apply_simple_replace() {
        let out = apply_patch("hello\nworld\n", "@@ -2 +2 @@\n-world\n+rust\n").unwrap();
        assert_eq!(out, "hello\nrust\n");
    }

    #[test]
    fn apply_reverse_order_hunks() {
        // Hunks carry overlapping original coordinates; reverse application
        // must keep both anchored to the original file layout.
        let base = "a\nx\nb\nx\nc\n";
        let diff = "@@ -1,3 +1,3 @@\n a\n-x\n+y\n b\n@@ -3,3 +3,3 @@\n b\n-x\n+z\n c\n";
        assert_eq!(apply_patch(base, diff).unwrap(), "a\ny\nb\nz\nc\n");
    }

    #[test]
    fn ambiguous_hunk_is_rejected() {
        let err = apply_patch("x\nx\n", "@@ -1 +1 @@\n-x\n+y\n").unwrap_err();
        assert!(matches!(err, PatchError::Ambiguous(_)));
    }

    #[test]
    fn mismatched_hunk_is_rejected() {
        let err = apply_patch("hello\n", "@@ -5 +5 @@\n-nope\n+yes\n").unwrap_err();
        assert!(matches!(err, PatchError::Mismatch(_)));
    }

    #[test]
    fn insertion_hunk_anchors_at_line_number() {
        let out = apply_patch("a\nc\n", "@@ -2,0 +2,1 @@\n+b\n").unwrap();
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn prepend_hunk_applies_to_existing_file() {
        // `-0,0` on an existing file is a legal header insertion, not a
        // creation conflict.
        let out = apply_patch("content\n", "@@ -0,0 +1,1 @@\n+head\n").unwrap();
        assert_eq!(out, "head\ncontent\n");
    }

    #[tokio::test]
    async fn creates_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let result = run(
            serde_json::json!({
                "path": path.to_string_lossy(),
                "diff": "@@ -0,0 +1,2 @@\n+line1\n+line2\n"
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "line1\nline2");
        let json = result.downcast::<serde_json::Value>().unwrap();
        let output = json["output"].as_str().unwrap();
        assert!(output.contains("patched"));
        assert!(output.contains("+line1"));
        assert!(output.contains("+line2"));
    }

    #[tokio::test]
    async fn prepend_on_existing_file_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exists.txt");
        std::fs::write(&path, "content\n").unwrap();
        let result = run(
            serde_json::json!({
                "path": path.to_string_lossy(),
                "diff": "@@ -0,0 +1,1 @@\n+head\n"
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "head\ncontent\n");
        let json = result.downcast::<serde_json::Value>().unwrap();
        let output = json["output"].as_str().unwrap();
        assert!(output.contains("patched"));
        assert!(output.contains("+head"));
    }

    #[tokio::test]
    async fn readonly_mode_denies() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(
            serde_json::json!({ "path": "a.txt", "diff": "@@ -1 +1 @@\n-x\n+y\n" }),
            dir.path().to_path_buf(),
            Some(SandboxMode::ReadOnly),
        )
        .await;
        assert!(
            matches!(result, Err(ToolError::Execution { code, .. }) if code == "READONLY_MODE")
        );
    }

    #[tokio::test]
    async fn failed_hunk_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let result = run(
            serde_json::json!({
                "path": path.to_string_lossy(),
                "diff": "@@ -1 +1 @@\n-hello\n+hi\n@@ -9 +9 @@\n-missing\n+gone\n"
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await;
        assert!(
            matches!(result, Err(ToolError::Execution { code, .. }) if code == "PATCH_MISMATCH")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
    }

    #[tokio::test]
    async fn missing_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(
            serde_json::json!({ "path": "nope.txt", "diff": "@@ -1 +1 @@\n-x\n+y\n" }),
            dir.path().to_path_buf(),
            None,
        )
        .await;
        assert!(matches!(result, Err(ToolError::Execution { code, .. }) if code == "NOT_FOUND"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn patch_preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let result = run(
            serde_json::json!({
                "path": path.to_string_lossy(),
                "diff": "@@ -1 +1 @@\n-x\n+y\n"
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        assert!(json["output"].as_str().unwrap().contains("patched"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn patch_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x\n").unwrap();
        let result = run(
            serde_json::json!({
                "path": path.to_string_lossy(),
                "diff": "@@ -1 +1 @@\n-x\n+y\n"
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "y\n");
        let json = result.downcast::<serde_json::Value>().unwrap();
        assert!(json["output"].as_str().unwrap().contains("patched"));
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "no temp files may remain after a patch");
    }

    #[tokio::test]
    async fn ambiguous_hunk_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x\nx\n").unwrap();
        let result = run(
            serde_json::json!({
                "path": path.to_string_lossy(),
                "diff": "@@ -1 +1 @@\n-x\n+y\n"
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await;
        assert!(
            matches!(result, Err(ToolError::Execution { code, .. }) if code == "AMBIGUOUS_PATCH")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\nx\n");
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "no temp files may remain after a failed patch"
        );
    }

    #[tokio::test]
    async fn rename_onto_directory_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("adir");
        std::fs::create_dir_all(&target).unwrap();
        let result = run(
            serde_json::json!({
                "path": target.to_string_lossy(),
                "diff": "@@ -0,0 +1,1 @@\n+x\n"
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await;
        assert!(matches!(result, Err(ToolError::Execution { code, .. }) if code == "WRITE_FAILED"));
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "failed rename must clean up the temp file"
        );
        assert!(target.is_dir());
    }

    /// G0: patching a file outside the workspace under workspace-write → OUTSIDE_WORKSPACE,
    /// the target is not modified.
    #[tokio::test]
    async fn patch_outside_workspace_rejected_under_workspace_write() {
        let dir = tempfile::tempdir().unwrap();
        let outside = crate::testutil::outside_boundary_dir("patch");
        let outside_file = outside.join("victim.txt");
        std::fs::write(&outside_file, "keep\n").unwrap();
        let err = run(
            serde_json::json!({
                "path": outside_file.to_string_lossy(),
                "diff": "@@ -1 +1 @@\n-keep\n+broken\n"
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
        assert_eq!(
            std::fs::read_to_string(&outside_file).unwrap(),
            "keep\n",
            "target file must be untouched"
        );
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// G0: targets inside the workspace patch normally (allowed within the boundary, no behavior regression).
    #[tokio::test]
    async fn patch_inside_workspace_allowed_under_workspace_write() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("in.txt"), "old\n").unwrap();
        run(
            serde_json::json!({
                "path": "in.txt",
                "diff": "@@ -1 +1 @@\n-old\n+new\n"
            }),
            dir.path().to_path_buf(),
            Some(SandboxMode::WorkspaceWrite),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("in.txt")).unwrap(),
            "new\n"
        );
    }

    // ── multi-file envelope ────────────────────────────────────────────────

    #[tokio::test]
    async fn multi_file_envelope_patches_all_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
        let result = run(
            serde_json::json!({
                "files": [
                    { "path": "a.txt", "diff": "@@ -1 +1 @@\n-alpha\n+ALPHA\n" },
                    { "path": "b.txt", "diff": "@@ -1 +1 @@\n-beta\n+BETA\n" }
                ]
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "ALPHA\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "BETA\n"
        );
        let json = result.downcast::<serde_json::Value>().unwrap();
        let output = json["output"].as_str().unwrap();
        assert!(output.contains("patched 2 files"), "{output}");
        assert!(output.contains("a.txt"), "{output}");
        assert!(output.contains("b.txt"), "{output}");
    }

    #[tokio::test]
    async fn mid_envelope_write_failure_rolls_back_modified_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        // b is a directory: staging sees a creation diff (hunks at -0,0) and
        // the write phase's rename onto a directory fails — after a.txt was
        // already written, forcing the rollback path.
        std::fs::create_dir_all(dir.path().join("b.txt")).unwrap();
        let err = run(
            serde_json::json!({
                "files": [
                    { "path": "a.txt", "diff": "@@ -1 +1 @@\n-alpha\n+ALPHA\n" },
                    { "path": "b.txt", "diff": "@@ -0,0 +1,1 @@\n+new\n" }
                ]
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "WRITE_FAILED");
        assert!(err.to_string().contains("rolled back"), "{err}");
        // a.txt restored to its original content.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\n"
        );
        // No temp files remain from either the failed write or the rollback.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(
            entries.len(),
            2,
            "only a.txt and the b.txt directory remain"
        );
    }

    #[tokio::test]
    async fn mid_envelope_failure_rolls_back_created_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("blocker")).unwrap();
        let err = run(
            serde_json::json!({
                "files": [
                    { "path": "created.txt", "diff": "@@ -0,0 +1,1 @@\n+fresh\n" },
                    { "path": "blocker", "diff": "@@ -0,0 +1,1 @@\n+new\n" }
                ]
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "WRITE_FAILED");
        assert!(
            !dir.path().join("created.txt").exists(),
            "created file must be removed by rollback"
        );
    }

    #[tokio::test]
    async fn envelope_path_escape_rejected_without_touching_victims() {
        let dir = tempfile::tempdir().unwrap();
        let outside = crate::testutil::outside_boundary_dir("patch-envelope");
        let victim = outside.join("victim.txt");
        std::fs::write(&victim, "keep\n").unwrap();
        std::fs::write(dir.path().join("in.txt"), "old\n").unwrap();
        let err = run(
            serde_json::json!({
                "files": [
                    { "path": "in.txt", "diff": "@@ -1 +1 @@\n-old\n+new\n" },
                    { "path": victim.to_string_lossy(), "diff": "@@ -1 +1 @@\n-keep\n+broken\n" }
                ]
            }),
            dir.path().to_path_buf(),
            Some(SandboxMode::WorkspaceWrite),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "OUTSIDE_WORKSPACE");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep\n");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("in.txt")).unwrap(),
            "old\n"
        );
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[tokio::test]
    async fn envelope_rejects_duplicate_paths_and_bad_forms() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let err = run(
            serde_json::json!({
                "files": [
                    { "path": "a.txt", "diff": "@@ -1 +1 @@\n-x\n+y\n" },
                    { "path": "a.txt", "diff": "@@ -1 +1 @@\n-y\n+z\n" }
                ]
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "x\n"
        );

        // Both forms at once / neither form / empty entries.
        for args in [
            serde_json::json!({ "path": "a.txt", "diff": "@@ -1 +1 @@\n-x\n+y\n",
                                "files": [{ "path": "a.txt", "diff": "@@ -1 +1 @@\n-x\n+y\n" }] }),
            serde_json::json!({}),
            serde_json::json!({ "files": [] }),
            serde_json::json!({ "files": [{ "path": "a.txt" }] }),
        ] {
            let err = run(args, dir.path().to_path_buf(), None).await.unwrap_err();
            assert_eq!(err.code(), "INVALID_INPUT");
        }
    }

    #[tokio::test]
    async fn envelope_stage_failure_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        // The second entry's hunk does not match: staging fails before any
        // write, so the first file must stay untouched.
        std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
        let err = run(
            serde_json::json!({
                "files": [
                    { "path": "a.txt", "diff": "@@ -1 +1 @@\n-alpha\n+ALPHA\n" },
                    { "path": "b.txt", "diff": "@@ -1 +1 @@\n-missing\n+new\n" }
                ]
            }),
            dir.path().to_path_buf(),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), "PATCH_MISMATCH");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "beta\n"
        );
    }
}


// ── BUG-92 regression (round 6) ──
#[cfg(test)]
mod round6_tests {
    use super::*;

    #[test]
    fn indented_context_lines_apply() {
        let base = "fn main() {\n    let x = 1;\n}\n";
        let diff = "@@ -1,3 +1,3 @@\n fn main() {\n     let x = 1;\n-}\n+}// end\n";
        let out = apply_patch(base, diff).expect("indented context must match");
        assert!(out.contains("}// end"), "the change lands (got {out:?})");
    }
}
