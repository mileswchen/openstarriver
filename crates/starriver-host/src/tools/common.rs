//! Shared helpers for tools rewritten on the kernel seam.

use std::path::{Path, PathBuf};

use cordis_rslite::Value;
use starriver_kernel::tools::ToolError;

/// Build a unified diff between `old` and `new` content for `path` — the
/// `--- path` / `+++ path` / `@@` format the frontend DiffView renders.
/// Returns `None` when the content is unchanged.
/// (Ported from core::tools::diff when that module was retired.)
pub fn unified_file_diff(path: &str, old: &str, new: &str) -> Option<String> {
    if old == new {
        return None;
    }
    let diff = similar::TextDiff::from_lines(old, new);
    Some(
        diff.unified_diff()
            .context_radius(3)
            .header(path, path)
            .to_string(),
    )
}

/// Parse the model arguments as a JSON object.
pub fn json_args(arguments: &Value) -> Result<serde_json::Value, ToolError> {
    // The agent loop carries model arguments as kernel JsonValue's runtime
    // spellings (Vec<(String, JsonValue)> for objects); tests may hand
    // serde_json::Value directly. Reconstruct a JsonValue, then convert.
    let json = match starriver_kernel::json::JsonValue::from_value(arguments) {
        Ok(json) => json,
        Err(_) => {
            let value = arguments.downcast::<serde_json::Value>().map_err(|_| {
                tool_failure(
                    "tool",
                    "INVALID_INPUT",
                    "arguments must be a JSON object".into(),
                )
            })?;
            if !value.is_object() {
                return Err(tool_failure(
                    "tool",
                    "INVALID_INPUT",
                    "arguments must be a JSON object".into(),
                ));
            }
            return Ok((*value).clone());
        }
    };
    let value = json
        .to_serde_json()
        .map_err(|e| tool_failure("tool", "INVALID_INPUT", format!("arguments not JSON: {e}")))?;
    if !value.is_object() {
        return Err(tool_failure(
            "tool",
            "INVALID_INPUT",
            "arguments must be a JSON object".into(),
        ));
    }
    Ok(value)
}

/// Resolve a possibly-relative path against the session cwd.
pub fn resolve_path(cwd: Option<&Path>, raw: &str) -> PathBuf {
    let base = cwd.unwrap_or_else(|| Path::new("."));
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Render a plain-text value into a content block (definition render
/// signature: canonical value plus the finalizer's value).
pub fn text_content(_canonical: &Value, value: &Value) -> Vec<starriver_kernel::llm::ContentBlock> {
    use starriver_kernel::llm::ContentBlock;
    let text = if let Ok(json) = value.downcast::<starriver_kernel::json::JsonValue>() {
        json.get("output")
            .and_then(starriver_kernel::json::JsonValue::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("{json:?}"))
    } else {
        value
            .downcast::<serde_json::Value>()
            .ok()
            .and_then(|v| v.get("output").and_then(|o| o.as_str()).map(String::from))
            .unwrap_or_else(|| format!("{value:?}"))
    };
    vec![ContentBlock::Text(text)]
}

/// Append the unified diff between `old` and `new` content to a tool output
/// string, in the `--- path` / `+++ path` / `@@` format the frontend DiffView
/// renders as filename + line numbers + additions/deletions. No-op when the
/// content is unchanged; oversized diffs are line-truncated so the output
/// stays under the spill threshold.
pub fn append_diff(output: &mut String, path: &str, old: &str, new: &str) {
    const MAX_DIFF_BYTES: usize = 16 * 1024;
    let Some(diff) = unified_file_diff(path, old, new) else {
        return;
    };
    output.push_str("\n\n");
    if diff.len() <= MAX_DIFF_BYTES {
        output.push_str(&diff);
        return;
    }
    let mut kept = 0usize;
    for line in diff.lines() {
        let line_len = line.len() + 1;
        if kept + line_len > MAX_DIFF_BYTES {
            break;
        }
        output.push_str(line);
        output.push('\n');
        kept += line_len;
    }
    output.push_str(&format!("(diff truncated, {} bytes total)", diff.len()));
}

/// Build an invalid-input failure with a stable code.
pub fn invalid_input(tool: &str, message: String) -> ToolError {
    tool_failure(tool, "INVALID_INPUT", message)
}

/// Build an execution failure.
pub fn tool_failure(tool: &str, code: &str, message: String) -> ToolError {
    ToolError::Execution {
        tool: tool.into(),
        code: code.into(),
        message,
    }
}

/// Atomic file write: same-directory temp file + rename, preserving the
/// original file's permissions when it exists. Shared by the file-mutating
/// tools (patch, lsp rename/codeAction apply).
pub async fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.starriver-write-{}-{}",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    tokio::fs::write(&tmp, contents).await?;
    if let Ok(meta) = tokio::fs::metadata(path).await {
        let _ = tokio::fs::set_permissions(&tmp, meta.permissions()).await;
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod diff_tests {
    use super::unified_file_diff;

    #[test]
    fn identical_content_has_no_diff() {
        assert_eq!(unified_file_diff("a.txt", "x\n", "x\n"), None);
    }

    #[test]
    fn changed_content_emits_unified_diff() {
        let d = unified_file_diff("a.txt", "one\ntwo\nthree\n", "one\nTWO\nthree\n").unwrap();
        assert!(d.starts_with("--- a.txt\n+++ a.txt\n"), "got: {d}");
        assert!(d.contains("@@"));
        assert!(d.contains("-two"));
        assert!(d.contains("+TWO"));
    }

    #[test]
    fn new_file_is_all_additions() {
        let d = unified_file_diff("a.txt", "", "hello\nworld\n").unwrap();
        assert!(d.contains("+hello"));
        assert!(d.contains("+world"));
        assert!(!d
            .lines()
            .any(|l| l.starts_with('-') && !l.starts_with("---")));
    }

    #[test]
    fn multibyte_lines_are_safe() {
        let d = unified_file_diff("a.txt", "中文一\n", "中文二\n").unwrap();
        assert!(d.contains("-中文一"));
        assert!(d.contains("+中文二"));
    }
}
