//! Tool-output spill (`spill` parity): oversized tool output is persisted
//! to `~/.starriver/spill/` and the in-context result is replaced by a short
//! summary with the file path, so the session log and the model context never
//! balloon with one giant output.

use std::path::PathBuf;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use starriver_kernel::llm::ContentBlock;
use starriver_kernel::tools::{ToolContentFinalizer, ToolExecutionResult};

/// Default spill threshold: tool outputs above this many text bytes are
/// persisted and replaced in-context.
pub const SPILL_THRESHOLD_BYTES: usize = 32 * 1024;

/// Attach the spill finalizer to every host tool whose output can grow.
/// Returns `None` when the output fits, so the canonical content is kept.
pub fn spill_finalizer() -> ToolContentFinalizer {
    spill_finalizer_with_threshold(SPILL_THRESHOLD_BYTES)
}

/// WI-10: spill with an explicit inline-bytes threshold (settings
/// `agent.spill_threshold_bytes`).
pub fn spill_finalizer_with_threshold(threshold_bytes: usize) -> ToolContentFinalizer {
    Arc::new(move |_exec, result: &ToolExecutionResult| {
        let threshold = threshold_bytes;
        let full: String = result
            .content()
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        if full.len() <= threshold {
            return None;
        }
        let mut hasher = Sha256::new();
        hasher.update(full.as_bytes());
        let sha: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        let dir = home.join(".starriver").join("spill");
        let path = dir.join(format!("{sha}.txt"));
        let _ = std::fs::create_dir_all(&dir);
        if std::fs::write(&path, &full).is_err() {
            return None; // Keep the output in-context if persistence fails.
        }
        let summary = format!(
            "(tool output truncated: {} chars — full output at {})",
            full.chars().count(),
            path.display()
        );
        // Keep a trailing bash exit-code marker visible after a spill: the
        // model checks it mechanically (bash:guide), and an oversized FAILING
        // command is exactly where the marker matters most.
        let summary = match exit_code_marker(full.trim_end()) {
            Some(marker) => format!("{summary}\n{marker}"),
            None => summary,
        };
        Some(vec![ContentBlock::Text(summary)])
    })
}

/// The trailing `[exit code: N]` line of a bash result, when present.
fn exit_code_marker(trimmed_end: &str) -> Option<&str> {
    let last = trimmed_end
        .rfind('\n')
        .map(|i| &trimmed_end[i + 1..])
        .unwrap_or(trimmed_end);
    let inner = last.strip_prefix('[')?.strip_suffix(']')?;
    inner.strip_prefix("exit code: ").map(|_| last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_rslite::Value;
    use starriver_kernel::tools::{
        mint_tool_execution_token, ToolExecution, ToolExecutionResult, ToolFailure,
    };

    fn result_with_text(text: String) -> ToolExecutionResult {
        ToolExecutionResult::Success {
            value: Value::new(()),
            content: vec![ContentBlock::Text(text)],
            additional_contexts: Vec::new(),
            concludes_turn: false,
        }
    }

    fn exec() -> ToolExecution {
        ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "bash".into(),
            arguments: Value::new(()),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: mint_tool_execution_token(),
            cancellation: None,
        }
    }

    #[test]
    fn small_output_stays_in_context() {
        let finalizer = spill_finalizer();
        let out = finalizer(&exec(), &result_with_text("small".into()));
        assert!(out.is_none());
    }

    /// A spilled bash result must keep its trailing exit-code marker — the
    /// model checks it mechanically, and oversized failing commands are the
    /// exact case where it matters.
    #[test]
    fn spilled_output_keeps_the_exit_code_marker() {
        crate::testutil::with_temp_home(|_dir| {
            let finalizer = spill_finalizer_with_threshold(64);
            let big = format!("{}\n[exit code: 3]", "y".repeat(80));
            let out = finalizer(&exec(), &result_with_text(big)).expect("spilled");
            let ContentBlock::Text(summary) = &out[0] else {
                panic!("summary text");
            };
            assert!(summary.contains("truncated"), "{summary}");
            assert!(summary.ends_with("[exit code: 3]"), "{summary}");
        });
    }

    #[test]
    fn oversized_output_spills_to_disk() {
        crate::testutil::with_temp_home(|dir| {
            let finalizer = spill_finalizer();
            let big = "x".repeat(SPILL_THRESHOLD_BYTES + 1);
            let out = finalizer(&exec(), &result_with_text(big)).expect("spilled");
            assert_eq!(out.len(), 1);
            let ContentBlock::Text(summary) = &out[0] else {
                panic!("summary text");
            };
            assert!(summary.contains("truncated"), "{summary}");
            // Native separators on Windows: compare on the normalized form.
            assert!(
                summary.replace('\\', "/").contains(".starriver/spill/"),
                "{summary}"
            );
            // The file actually exists under the isolated HOME.
            let spill_dir = dir.join(".starriver").join("spill");
            let entries: Vec<_> = std::fs::read_dir(&spill_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .collect();
            assert_eq!(entries.len(), 1);
        });
    }

    #[test]
    fn error_results_are_never_spilled() {
        let finalizer = spill_finalizer();
        let result = ToolExecutionResult::Failure {
            error: ToolFailure {
                message: "boom".into(),
                code: "X".into(),
            },
            content: vec![ContentBlock::Text("err".into())],
            additional_contexts: Vec::new(),
        };
        assert!(finalizer(&exec(), &result).is_none());
    }
}
