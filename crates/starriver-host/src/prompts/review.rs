use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewOptions {
    pub files: Vec<String>,
    pub dimensions: Vec<String>,
}

/// WI-96: structured review target — what to review, not just which files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ReviewTarget {
    /// All uncommitted changes in the working tree.
    Uncommitted,
    /// Diff against a base branch (merge-base).
    BaseBranch { branch: String },
    /// One commit's changes.
    Commit { sha: String },
    /// Explicit file list (the legacy form).
    Files { files: Vec<String> },
}

impl ReviewTarget {
    /// Resolve the target to a file list + the unified diffs to review,
    /// running git in `cwd`. `Files` needs no git.
    pub fn resolve(&self, cwd: &std::path::Path) -> Result<(Vec<String>, String), String> {
        let git = |args: &[&str]| -> Result<String, String> {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .output()
                .map_err(|e| format!("git unavailable: {e}"))?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        };
        match self {
            ReviewTarget::Files { files } => Ok((files.clone(), String::new())),
            ReviewTarget::Uncommitted => {
                let diff = git(&["diff", "HEAD"])?;
                let files = git(&["diff", "--name-only", "HEAD"])?;
                let list: Vec<String> = files
                    .lines()
                    .map(str::to_string)
                    .filter(|l| !l.is_empty())
                    .collect();
                Ok((list, diff))
            }
            ReviewTarget::BaseBranch { branch } => {
                let base = git(&["merge-base", branch, "HEAD"])
                    .map_err(|e| format!("merge-base {branch}: {e}"))?;
                let base = base.trim();
                if base.is_empty() {
                    return Err(format!("no merge-base with {branch}"));
                }
                let diff = git(&["diff", base, "HEAD"])?;
                let files = git(&["diff", "--name-only", base, "HEAD"])?;
                let list: Vec<String> = files
                    .lines()
                    .map(str::to_string)
                    .filter(|l| !l.is_empty())
                    .collect();
                Ok((list, diff))
            }
            ReviewTarget::Commit { sha } => {
                let diff = git(&["show", "--format=", sha])?;
                let files = git(&["show", "--name-only", "--format=", sha])?;
                let list: Vec<String> = files
                    .lines()
                    .map(str::to_string)
                    .filter(|l| !l.is_empty())
                    .collect();
                Ok((list, diff))
            }
        }
    }
}

/// WI-96: one structured review finding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewFinding {
    pub title: String,
    pub body: String,
    /// 0 = high, 1 = medium, 2 = low.
    pub priority: u8,
    /// 0.0..=1.0.
    pub confidence: f64,
    pub file: String,
    pub line_start: u64,
    pub line_end: u64,
}

/// WI-96: parse the reviewer's JSON output (fenced or bare) into findings.
pub fn parse_review_findings(text: &str) -> Vec<ReviewFinding> {
    let start = text.find('{');
    let end = text.rfind('}');
    let (Some(start), Some(end)) = (start, end) else {
        return Vec::new();
    };
    let slice = &text[start..=end];
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)]
        findings: Vec<RawFinding>,
    }
    #[derive(serde::Deserialize)]
    struct RawFinding {
        #[serde(default)]
        title: String,
        #[serde(default)]
        body: String,
        // BUG-93: a missing priority must not clamp to 0 (= high); the
        // neutral default is 1 (medium). Confidence keeps its 0.0 default —
        // an unspecified confidence is low, an unspecified priority is not.
        #[serde(default)]
        priority: Option<f64>,
        #[serde(default)]
        confidence: f64,
        #[serde(default)]
        file: String,
        #[serde(default, alias = "lineStart")]
        line_start: u64,
        #[serde(default, alias = "lineEnd")]
        line_end: u64,
    }
    let Ok(raw) = serde_json::from_str::<Raw>(slice) else {
        return Vec::new();
    };
    raw.findings
        .into_iter()
        .map(|f| ReviewFinding {
            title: f.title,
            body: f.body,
            priority: f.priority.unwrap_or(1.0).clamp(0.0, 2.0) as u8,
            confidence: f.confidence.clamp(0.0, 1.0),
            file: f.file,
            line_start: f.line_start,
            line_end: if f.line_end >= f.line_start {
                f.line_end
            } else {
                f.line_start
            },
        })
        .collect()
}

/// WI-96: the structured-review prompt (child-thread one-shot).
pub fn structured_review_prompt(
    files: &[String],
    diff: &str,
    dimensions: &[String],
) -> String {
    let dim_line = if dimensions.is_empty() {
        "correctness, security, performance, maintainability, testing".to_string()
    } else {
        dimensions.join(", ")
    };
    let mut prompt = format!(
        "Review the following changes. Dimensions: {dim_line}.\n\nFiles:\n{}\n\n",
        files.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n")
    );
    if !diff.is_empty() {
        prompt.push_str("Unified diff:\n```diff\n");
        prompt.push_str(diff);
        prompt.push_str("\n```\n\n");
    }
    prompt.push_str("Output ONLY a JSON object (no prose outside it):\n");
    prompt.push_str(
        r#"{"findings": [{"title": "...", "body": "...", "priority": 0, "confidence": 0.9, "file": "path", "lineStart": 1, "lineEnd": 5}]}"#,
    );
    prompt.push_str(
        "\npriority: 0=high 1=medium 2=low. lineStart/lineEnd refer to the NEW file side of the diff. If nothing is wrong, output {\"findings\": []}.",
    );
    prompt
}

pub fn review_system_prompt(options: &ReviewOptions) -> String {
    let dims: Vec<&str> = if options.dimensions.is_empty() {
        vec![
            "correctness",
            "security",
            "performance",
            "maintainability",
            "testing",
        ]
    } else {
        options.dimensions.iter().map(|s| s.as_str()).collect()
    };
    let dim_desc: String = dims
        .iter()
        .map(|d| match *d {
            "correctness" => "- **Correctness**: logic errors, edge cases, exception handling",
            "security" => "- **Security**: SQL injection, XSS, permission issues, secret leaks",
            "performance" => {
                "- **Performance**: N+1 queries, memory leaks, unnecessary allocations"
            }
            "maintainability" => {
                "- **Maintainability**: code smells, naming conventions, duplicate code"
            }
            "testing" => "- **Testing**: test coverage, edge cases, testability",
            _ => "",
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"You are a code review expert. Please review the following files.

## Review dimensions

{dim_desc}

## Review files

{files}

## Output requirements

Categorized by severity: critical > major > minor > suggestion

Each issue includes:
1. File path and line number
2. Issue description
3. Suggested fix"#,
        dim_desc = dim_desc,
        files = options.files.join(", ")
    )
}

pub fn review_task_prompt(files: &[String]) -> String {
    format!("Please review the following files: {}", files.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── review_system_prompt tests ──

    #[test]
    fn empty_dimensions_uses_default_five() {
        let opts = ReviewOptions {
            files: vec!["src/auth.ts".into()],
            dimensions: vec![],
        };
        let prompt = review_system_prompt(&opts);
        assert!(prompt.contains("Correctness"));
        assert!(prompt.contains("Security"));
        assert!(prompt.contains("Performance"));
        assert!(prompt.contains("Maintainability"));
        assert!(prompt.contains("Testing"));
        assert!(prompt.contains("src/auth.ts"));
    }

    #[test]
    fn custom_dimensions_only_includes_specified() {
        let opts = ReviewOptions {
            files: vec!["src/lib.rs".into()],
            dimensions: vec!["correctness".into(), "security".into()],
        };
        let prompt = review_system_prompt(&opts);
        assert!(prompt.contains("Correctness"));
        assert!(prompt.contains("Security"));
        assert!(
            !prompt.contains("Performance"),
            "should not include unspecified dimension"
        );
        assert!(!prompt.contains("Maintainability"));
        assert!(!prompt.contains("Testing"));
    }

    #[test]
    fn single_dimension() {
        let opts = ReviewOptions {
            files: vec!["a.ts".into()],
            dimensions: vec!["performance".into()],
        };
        let prompt = review_system_prompt(&opts);
        assert!(prompt.contains("Performance"));
        assert!(!prompt.contains("Correctness"));
    }

    #[test]
    fn unknown_dimension_is_filtered_out() {
        let opts = ReviewOptions {
            files: vec!["x.ts".into()],
            dimensions: vec!["unknown-dim".into(), "correctness".into()],
        };
        let prompt = review_system_prompt(&opts);
        assert!(prompt.contains("Correctness"));
        // "unknown-dim" should map to "" which is filtered out
        assert!(
            !prompt.contains("unknown-dim"),
            "unknown dimension should be filtered"
        );
    }

    #[test]
    fn all_unknown_dimensions_produces_empty_dim_desc() {
        let opts = ReviewOptions {
            files: vec!["x.ts".into()],
            dimensions: vec!["foo".into(), "bar".into()],
        };
        let prompt = review_system_prompt(&opts);
        // Should still have the header structure even with empty dims
        assert!(prompt.contains("Review dimensions"));
        assert!(prompt.contains("x.ts"));
    }

    #[test]
    fn multiple_files_listed() {
        let opts = ReviewOptions {
            files: vec!["a.ts".into(), "b.rs".into(), "c.py".into()],
            dimensions: vec![],
        };
        let prompt = review_system_prompt(&opts);
        assert!(prompt.contains("a.ts"));
        assert!(prompt.contains("b.rs"));
        assert!(prompt.contains("c.py"));
    }

    #[test]
    fn empty_files_list() {
        let opts = ReviewOptions {
            files: vec![],
            dimensions: vec![],
        };
        let prompt = review_system_prompt(&opts);
        // Should not panic with empty files
        assert!(prompt.contains("Review files"));
    }

    #[test]
    fn output_requirements_present() {
        let opts = ReviewOptions {
            files: vec!["f.ts".into()],
            dimensions: vec![],
        };
        let prompt = review_system_prompt(&opts);
        assert!(prompt.contains("critical > major > minor > suggestion"));
        assert!(prompt.contains("File path and line number"));
    }

    // ── review_task_prompt tests ──

    #[test]
    fn single_file_task_prompt() {
        let result = review_task_prompt(&["src/main.rs".into()]);
        assert_eq!(result, "Please review the following files: src/main.rs");
    }

    #[test]
    fn multiple_files_task_prompt() {
        let result = review_task_prompt(&["a.ts".into(), "b.ts".into()]);
        assert_eq!(result, "Please review the following files: a.ts, b.ts");
    }

    #[test]
    fn empty_files_task_prompt() {
        let result = review_task_prompt(&[]);
        assert_eq!(result, "Please review the following files: ");
    }

    // ── Edge cases ──

    #[test]
    fn review_prompt_with_many_files() {
        let files: Vec<String> = (1..50).map(|i| format!("src/file{}.ts", i)).collect();
        let opts = ReviewOptions {
            files: files.clone(),
            dimensions: vec![],
        };
        let prompt = review_system_prompt(&opts);
        assert!(prompt.contains("file1.ts"));
        assert!(prompt.contains("file49.ts"));
    }

    #[test]
    fn review_prompt_no_crash_on_special_file_names() {
        let opts = ReviewOptions {
            files: vec![
                "src/file (1).ts".into(),
                "path/with spaces/file.ts".into(),
                "中文文件.rs".into(),
            ],
            dimensions: vec![],
        };
        let prompt = review_system_prompt(&opts);
        assert!(prompt.contains("src/file (1).ts"));
        assert!(prompt.contains("中文文件.rs"));
    }

    #[test]
    fn review_task_prompt_with_special_file_names() {
        let result = review_task_prompt(&["file with spaces.ts".into(), "中文.rs".into()]);
        assert_eq!(
            result,
            "Please review the following files: file with spaces.ts, 中文.rs"
        );
    }

    #[test]
    fn review_prompt_with_hundred_files() {
        let files: Vec<String> = (1..=100)
            .map(|i| format!("src/module{}/file{}.ts", i % 10, i))
            .collect();
        let opts = ReviewOptions {
            files: files.clone(),
            dimensions: vec![],
        };
        let prompt = review_system_prompt(&opts);
        assert!(prompt.contains("file1.ts"));
        assert!(prompt.contains("file100.ts"));
    }

    #[test]
    fn review_prompt_duplicate_files() {
        let opts = ReviewOptions {
            files: vec!["a.ts".into(), "a.ts".into(), "a.ts".into()],
            dimensions: vec![],
        };
        let prompt = review_system_prompt(&opts);
        // Repeated files should not crash
        assert!(prompt.contains("a.ts"));
    }

    #[test]
    fn review_prompt_empty_dimensions_is_valid() {
        let opts = ReviewOptions {
            files: vec!["f.ts".into()],
            dimensions: vec![],
        };
        let prompt = review_system_prompt(&opts);
        assert!(prompt.contains("Review dimensions"));
        assert!(prompt.contains("Correctness"));
    }

    #[test]
    fn review_prompt_all_known_dimensions() {
        let dims = vec![
            "correctness".into(),
            "security".into(),
            "performance".into(),
            "maintainability".into(),
            "testing".into(),
        ];
        let opts = ReviewOptions {
            files: vec!["f.ts".into()],
            dimensions: dims,
        };
        let prompt = review_system_prompt(&opts);
        for dim in &[
            "Correctness",
            "Security",
            "Performance",
            "Maintainability",
            "Testing",
        ] {
            assert!(prompt.contains(dim), "prompt should contain {}", dim);
        }
    }

    #[test]
    fn review_prompt_mixed_known_and_unknown_dimensions() {
        let opts = ReviewOptions {
            files: vec!["f.ts".into()],
            dimensions: vec!["correctness".into(), "foobar".into(), "security".into()],
        };
        let prompt = review_system_prompt(&opts);
        assert!(prompt.contains("Correctness"));
        assert!(prompt.contains("Security"));
        assert!(!prompt.contains("foobar"));
    }

    #[test]
    fn review_prompt_very_long_file_path() {
        let long_path = "a/".repeat(50) + "file.ts";
        let opts = ReviewOptions {
            files: vec![long_path.clone()],
            dimensions: vec![],
        };
        let prompt = review_system_prompt(&opts);
        assert!(prompt.contains("file.ts"));
    }
}
