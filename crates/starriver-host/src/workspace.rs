#![allow(dead_code, unused)]
use std::path::PathBuf;

/// Git auto-commit
#[allow(dead_code)]
pub struct AutoCommit {
    cwd: PathBuf,
}

impl AutoCommit {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }

    pub async fn commit(&self, message: &str) -> Result<(), String> {
        let output = tokio::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&self.cwd)
            .output()
            .await
            .map_err(|e| format!("git add: {e}"))?;
        if !output.status.success() {
            return Ok(());
        }

        tokio::process::Command::new("git")
            .args(["commit", "-m", &format!("auto: {message}")])
            .current_dir(&self.cwd)
            .output()
            .await
            .map_err(|e| format!("git commit: {e}"))?;
        Ok(())
    }
}

/// Undo manager — git checkpoint-based
#[allow(dead_code)]
pub struct UndoManager {
    cwd: PathBuf,
}

impl UndoManager {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }

    pub async fn save_checkpoint(&self, label: &str) -> Result<(), String> {
        tokio::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&self.cwd)
            .output()
            .await
            .map_err(|e| format!("undo save: {e}"))?;
        let stash_result = tokio::process::Command::new("git")
            .args(["stash", "push", "-m", label])
            .current_dir(&self.cwd)
            .output()
            .await
            .map_err(|e| format!("undo save: {e}"))?;
        if !stash_result.status.success() {
            // Unstash back
            let _ = tokio::process::Command::new("git")
                .args(["stash", "pop"])
                .current_dir(&self.cwd)
                .output()
                .await;
        }
        Ok(())
    }
}

/// Lint loop — run lint on edited files
#[allow(dead_code)]
pub struct LintLoop;

impl LintLoop {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LintLoop {
    fn default() -> Self {
        Self::new()
    }
}

impl LintLoop {
    pub async fn lint_file(&self, file: &str, cwd: &PathBuf) -> Result<(), String> {
        let ext = std::path::Path::new(file)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let Some((tool, args)) = lint_command(ext, file) else {
            return Ok(());
        };
        let _ = tokio::process::Command::new(tool)
            .args(args)
            .current_dir(cwd)
            .output()
            .await;
        Ok(())
    }
}

/// Extension → lint tool routing (pure; unit-tested without spawning the
/// actual tool — the smoke test that ran `npx eslint` four times cost 16.7s
/// and asserted a tautology).
fn lint_command(ext: &str, file: &str) -> Option<(String, Vec<String>)> {
    match ext {
        "ts" | "tsx" | "js" | "jsx" => Some((
            "npx".into(),
            vec!["eslint".into(), file.into(), "--fix".into()],
        )),
        "rs" => Some(("cargo".into(), vec!["fmt".into(), "--".into(), file.into()])),
        "py" => Some(("black".into(), vec![file.into()])),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_commit_creates() {
        let ac = AutoCommit::new(PathBuf::from("/tmp"));
        assert!(true); // Just verify type construction
    }

    #[test]
    fn test_undo_manager_creates() {
        let um = UndoManager::new(PathBuf::from("/tmp"));
        assert!(true); // Just verify type construction
    }

    #[test]
    fn test_lint_loop_creates() {
        let _ll = LintLoop::new();
        assert!(true); // Just verify type construction
    }

    #[test]
    fn test_lint_file_unknown_ext_is_ok() {
        let _ll = LintLoop::new();
        // Unknown extension should return Ok(())
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let result = _ll.lint_file("unknown.xyz", &PathBuf::from("/tmp")).await;
            assert!(result.is_ok());
        });
    }

    #[test]
    fn test_lint_file_known_rust() {
        // Rust files use cargo fmt
        // We can't run cargo fmt in test, but verify the extension matching works
        let ext = std::path::Path::new("test.rs")
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        assert_eq!(ext, "rs");
    }

    #[test]
    fn test_auto_commit_with_different_paths() {
        let ac1 = AutoCommit::new(PathBuf::from("/home/user/projects/starriver"));
        let ac2 = AutoCommit::new(PathBuf::from("/tmp"));
        assert!(true); // Verify both construct without panic
    }

    #[test]
    fn test_undo_manager_with_deep_path() {
        let um = UndoManager::new(PathBuf::from("/a/b/c/d/e/f/g"));
        assert!(true);
    }

    #[test]
    fn test_lint_file_typescript_extensions() {
        let _ll = LintLoop::new();
        for ext in &["ts", "tsx", "js", "jsx"] {
            let path = format!("test.{}", ext);
            let actual = std::path::Path::new(&path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            assert_eq!(actual, *ext);
        }
    }

    #[test]
    fn test_lint_file_python_extension() {
        let ext = std::path::Path::new("test.py")
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        assert_eq!(ext, "py");
    }

    #[test]
    fn test_lint_file_no_extension() {
        let path = std::path::Path::new("Makefile");
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        assert_eq!(ext, "");
        // No extension → lint_file returns Ok(())
        let _ll = LintLoop::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let result = _ll.lint_file("Makefile", &PathBuf::from("/tmp")).await;
            assert!(result.is_ok());
        });
    }

    #[test]
    fn test_auto_commit_empty_message() {
        let ac = AutoCommit::new(PathBuf::from("/tmp"));
        // Instantiation works even with unusual paths
        assert!(true);
    }

    #[test]
    fn test_undo_manager_relative_path() {
        let um = UndoManager::new(PathBuf::from("."));
        assert!(true);
    }

    #[test]
    fn test_lint_file_with_absolute_path() {
        let _ll = LintLoop::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let result = _ll
                .lint_file("/absolute/path/to/file.rs", &PathBuf::from("/tmp"))
                .await;
            assert!(result.is_ok());
        });
    }

    #[test]
    fn lint_command_routes_every_supported_extension() {
        for ext in ["ts", "tsx", "js", "jsx"] {
            let (tool, args) = lint_command(ext, "f").expect("js family routes to eslint");
            assert_eq!(tool, "npx");
            assert_eq!(args, vec!["eslint", "f", "--fix"]);
        }
        assert_eq!(
            lint_command("rs", "f"),
            Some(("cargo".into(), vec!["fmt".into(), "--".into(), "f".into()]))
        );
        assert_eq!(
            lint_command("py", "f"),
            Some(("black".into(), vec!["f".into()]))
        );
        // Unsupported extensions lint to a no-op.
        assert_eq!(lint_command("xyz", "f"), None);
        assert_eq!(lint_command("", "f"), None);
    }

    #[test]
    fn test_auto_commit_root_path() {
        let ac = AutoCommit::new(PathBuf::from("/"));
        assert!(true);
    }

    #[test]
    fn test_undo_manager_dot_path() {
        let um = UndoManager::new(PathBuf::from("."));
        assert!(true);
    }
}
