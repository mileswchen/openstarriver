//! Grep tool: ripgrep-grade search built on the ripgrep kernel crates —
//! `ignore` (gitignore-aware parallel walk), `grep-regex` + `grep-searcher`
//! (line matching with binary detection). Replaces the earlier sequential
//! walkdir implementation that ignored .gitignore and could not see context.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cordis_rslite::Value;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, json_args, resolve_path, text_content, tool_failure};

/// Hard cap on output lines (matches + context) across the whole search —
/// keeps a hot pattern from flooding the model with megabytes.
const MAX_MATCH_LINES: usize = 500;

/// Per-thread visitor: each file runs its own searcher with the shared
/// collector; the global line budget quits the walk when exhausted.
struct SearchBuilder {
    matcher: grep_regex::RegexMatcher,
    include: Option<globset::GlobSet>,
    context: usize,
    collector: Collector,
    root: PathBuf,
}

/// One thread's visitor state.
struct SearchVisitor {
    matcher: grep_regex::RegexMatcher,
    include: Option<globset::GlobSet>,
    context: usize,
    collector: Collector,
    root: PathBuf,
}

impl ignore::ParallelVisitor for SearchVisitor {
    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> ignore::WalkState {
        let Ok(entry) = entry else {
            return ignore::WalkState::Continue;
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            return ignore::WalkState::Continue;
        }
        if self.collector.budget.load(Ordering::Relaxed) == 0 {
            return ignore::WalkState::Quit;
        }
        let path = entry.into_path();
        if let Some(include) = &self.include {
            let name_hit = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| include.is_match(name));
            // Directory-scoped globs ("src/*.rs") are relative patterns:
            // match them against the path RELATIVE to the search root —
            // GlobSet::is_match is full-match, so the absolute walker path
            // can never hit them.
            let path_hit = path
                .strip_prefix(&self.root)
                .ok()
                .and_then(|rel| rel.to_str())
                .is_some_and(|text| include.is_match(text))
                || path.to_str().is_some_and(|text| include.is_match(text));
            if !name_hit && !path_hit {
                return ignore::WalkState::Continue;
            }
        }
        let mut searcher = grep_searcher::SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .before_context(self.context)
            .after_context(self.context)
            .build();
        let sink = FileSink {
            path: path.clone(),
            collector: self.collector.clone(),
        };
        let _ = searcher.search_path(&self.matcher, &path, sink);
        if self.collector.budget.load(Ordering::Relaxed) == 0 {
            ignore::WalkState::Quit
        } else {
            ignore::WalkState::Continue
        }
    }
}

impl<'s> ignore::ParallelVisitorBuilder<'s> for SearchBuilder {
    fn build(&mut self) -> Box<dyn ignore::ParallelVisitor + 's> {
        Box::new(SearchVisitor {
            matcher: self.matcher.clone(),
            include: self.include.clone(),
            context: self.context,
            collector: self.collector.clone(),
            root: self.root.clone(),
        })
    }
}

struct Grep;

/// Shared sink state for one search: matched lines land here from every
/// parallel walker thread, then get sorted for deterministic output.
#[derive(Clone)]
struct Collector {
    /// (path, line, separator, text) — structured so the final sort is
    /// (path, numeric line), not lexicographic "path:10 < path:2".
    hits: Arc<Mutex<Vec<(String, u64, char, String)>>>,
    budget: Arc<AtomicUsize>,
    has_context: bool,
}

struct FileSink {
    path: PathBuf,
    collector: Collector,
}

impl FileSink {
    fn push(&self, separator: char, line_number: u64, line: &str) -> bool {
        // Decrement without wrapping: at budget 0 a plain fetch_sub wraps to
        // usize::MAX, and under parallel visitors every later check then
        // passes — the 500-line hard cap stops bounding anything.
        let mut remaining = self.collector.budget.load(Ordering::Relaxed);
        loop {
            if remaining == 0 {
                return false;
            }
            match self.collector.budget.compare_exchange_weak(
                remaining,
                remaining - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => remaining = actual,
            }
        }
        let text = line.trim_end_matches(['\n', '\r']);
        self.collector
            .hits
            .lock()
            .expect("grep hits poisoned")
            .push((self.path.display().to_string(), line_number, separator, text.to_string()));
        self.collector.budget.load(Ordering::Relaxed) > 0
    }
}

impl Sink for FileSink {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch) -> Result<bool, Self::Error> {
        Ok(self.push(
            ':',
            mat.line_number().unwrap_or(0),
            &String::from_utf8_lossy(mat.bytes()),
        ))
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext,
    ) -> Result<bool, Self::Error> {
        if !self.collector.has_context {
            return Ok(true);
        }
        Ok(self.push(
            '-',
            context.line_number().unwrap_or(0),
            &String::from_utf8_lossy(context.bytes()),
        ))
    }
}

impl ToolExecutor for Grep {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let pattern = json
                .get("pattern")
                .and_then(|p| p.as_str())
                .ok_or_else(|| invalid_input("grep", "missing pattern".into()))?;
            let matcher = RegexMatcherBuilder::new()
                .multi_line(false)
                .build(pattern)
                .map_err(|e| {
                    tool_failure("grep", "INVALID_REGEX", format!("invalid regex: {e}"))
                })?;
            let base = ctx.execution.cwd.clone().unwrap_or_default();
            let search_path = json
                .get("path")
                .and_then(|p| p.as_str())
                .map(|p| resolve_path(ctx.execution.cwd.as_deref(), p))
                .unwrap_or(base);
            let include = match json.get("include").and_then(|g| g.as_str()) {
                Some(text) if !text.is_empty() => {
                    let mut builder = globset::GlobSetBuilder::new();
                    for alternative in text.split(',') {
                        let glob = globset::GlobBuilder::new(alternative.trim())
                            .literal_separator(true)
                            .build()
                            .map_err(|e| {
                                tool_failure(
                                    "grep",
                                    "INVALID_GLOB",
                                    format!("invalid include glob: {e}"),
                                )
                            })?;
                        builder.add(glob);
                    }
                    Some(builder.build().map_err(|e| {
                        tool_failure("grep", "INVALID_GLOB", format!("invalid include glob: {e}"))
                    })?)
                }
                _ => None,
            };
            let context = json.get("context").and_then(|c| c.as_u64()).unwrap_or(0) as usize;
            let collector = Collector {
                hits: Arc::new(Mutex::new(Vec::new())),
                budget: Arc::new(AtomicUsize::new(MAX_MATCH_LINES)),
                has_context: context > 0,
            };
            let mut builder = SearchBuilder {
                matcher,
                include,
                context,
                collector: collector.clone(),
                root: search_path.clone(),
            };
            // require_git(false): workspaces without a .git directory still
            // honor their .gitignore, which is what the model expects.
            WalkBuilder::new(&search_path)
                .require_git(false)
                .filter_entry(|entry| {
                    !entry.file_type().is_some_and(|t| t.is_dir())
                        || !matches!(
                            entry.file_name().to_str(),
                            Some("target") | Some("node_modules") | Some(".git")
                        )
                })
                .build_parallel()
                .visit(&mut builder);
            let mut hits = collector.hits.lock().expect("grep hits poisoned").clone();
            // Ascending line order per file (ripgrep semantics): sorting the
            // rendered "path:line:text" strings made line 10 precede line 2.
            hits.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
            let output = if hits.is_empty() {
                "no matches".to_string()
            } else {
                hits.iter()
                    .map(|(path, line, separator, text)| format!("{path}{separator}{line}{separator}{text}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok(Value::new(serde_json::json!({ "output": output })))
        })
    }
}

/// tool-guide parity (`tool:grep` → `grep:guide` @ 150).
pub fn guide() -> (&'static str, i32, &'static str) {
    (
        "grep",
        150,
        "Use the grep tool — not shell grep or rg — to search file contents. It respects .gitignore, skips binary files, and returns path:line:text matches; follow a hit with read when you need surrounding context.",
    )
}

pub fn grep_definition() -> ToolDefinition {
    ToolDefinition {
        name: "grep".into(),
        description: "Search files with ripgrep semantics: respects .gitignore, skips hidden files and binary content, parallel walk. Parameters: pattern (regex), path (optional directory), include (optional glob on file paths, comma-separated), context (optional lines of context around each match). Output lines are path:line:text (context lines use path-line:text).".into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Search pattern" },
                "path": { "type": "string" },
                "include": { "type": "string", "description": "glob on file paths, e.g. *.rs or *.ts,*.tsx" },
                "context": { "type": "integer", "description": "context lines before and after each match" }
            },
            "required": ["pattern"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(Grep),
        is_concurrency_safe: Some(Arc::new(|_args| true)),
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

    async fn grep_at(
        dir: &std::path::Path,
        arguments: serde_json::Value,
    ) -> Result<String, ToolError> {
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "grep".into(),
            arguments: Value::new(serde_json::Value::Null),
            agent_id: None,
            cwd: Some(dir.to_path_buf()),
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(execution);
        let result = grep_definition()
            .executor
            .execute(Value::new(arguments), ctx)
            .await?;
        let json = result.downcast::<serde_json::Value>().unwrap();
        Ok(json["output"].as_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn grep_finds_matching_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "foo\nbar foo\nbaz").unwrap();
        let output = grep_at(dir.path(), serde_json::json!({ "pattern": "foo" }))
            .await
            .unwrap();
        assert!(output.contains("a.txt:1:foo"), "{output}");
        assert!(output.contains("a.txt:2:bar foo"), "{output}");
    }

    #[tokio::test]
    async fn gitignore_and_ignored_dirs_are_respected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "let needle = 1;").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\ntarget/\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "let needle = 2;").unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/stale.rs"), "let needle = 3;").unwrap();
        let output = grep_at(dir.path(), serde_json::json!({ "pattern": "needle" }))
            .await
            .unwrap();
        assert!(output.contains("keep.rs"), "{output}");
        assert!(!output.contains("ignored.rs"), "{output}");
        assert!(!output.contains("target"), "{output}");
    }

    #[tokio::test]
    async fn include_filters_by_file_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "needle").unwrap();
        std::fs::write(dir.path().join("b.txt"), "needle").unwrap();
        let output = grep_at(
            dir.path(),
            serde_json::json!({ "pattern": "needle", "include": "*.rs" }),
        )
        .await
        .unwrap();
        assert!(output.contains("a.rs"), "{output}");
        assert!(!output.contains("b.txt"), "{output}");
    }

    #[tokio::test]
    async fn context_lines_are_reported_with_dash_separator() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo needle\nthree").unwrap();
        let output = grep_at(
            dir.path(),
            serde_json::json!({ "pattern": "needle", "context": 1 }),
        )
        .await
        .unwrap();
        assert!(output.contains("a.txt:2:two needle"), "{output}");
        assert!(output.contains("a.txt-1-one"), "{output}");
        assert!(output.contains("a.txt-3-three"), "{output}");
    }

    #[tokio::test]
    async fn binary_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blob.bin"), b"needle\x00needle".to_vec()).unwrap();
        let output = grep_at(dir.path(), serde_json::json!({ "pattern": "needle" }))
            .await
            .unwrap();
        assert_eq!(output, "no matches");
    }

    #[tokio::test]
    async fn invalid_regex_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let err = grep_at(dir.path(), serde_json::json!({ "pattern": "(unclosed" }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "INVALID_REGEX");
    }
}
