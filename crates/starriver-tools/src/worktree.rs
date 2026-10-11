//! Git worktree engine — shared by UI queries and agent tools; state stays consistent.

use std::path::{Path, PathBuf};

/// One record from `git worktree list --porcelain`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>, // None = detached HEAD
    pub head: Option<String>,
    pub detached: bool,
    pub is_main: bool,
}

/// Git status of a directory (a non-git directory has is_repo=false, no error).
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorktreeStatus {
    pub is_repo: bool,
    pub branch: Option<String>,
    pub is_worktree: bool,
    pub worktree_path: Option<PathBuf>,
    pub dirty: bool,
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("git {args:?} failed: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// `canonicalize` yields `\\?\C:\…` device paths on Windows; git rejects
/// them ("could not create leading directories … Invalid argument"), so
/// canonical results are stripped back to the plain drive form.
fn plain_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s))
}

/// Resolve the main repository root from any checkout inside it (a linked
/// worktree or the main tree itself) via `git rev-parse --git-common-dir`.
/// A2a closeout: the worktree registry is derived from git instead of a table.
pub fn main_repo_of(cwd: &Path) -> Result<PathBuf, String> {
    let out = run_git(cwd, &["rev-parse", "--git-common-dir"])?;
    let common = PathBuf::from(out.trim());
    let common = if common.is_absolute() {
        common
    } else {
        cwd.join(common)
    };
    let root = plain_path(
        &common
            .canonicalize()
            .map_err(|e| format!("canonicalize git-common-dir: {e}"))?,
    )
    .parent()
    .ok_or("git-common-dir has no parent")?
    .to_path_buf();
    Ok(root)
}

/// Parse `git worktree list --porcelain`.
pub fn list(repo: &Path) -> Result<Vec<WorktreeInfo>, String> {
    let out = run_git(repo, &["worktree", "list", "--porcelain"])?;
    let mut infos = Vec::new();
    let mut main_seen = false;
    for block in out.split("\n\n") {
        if block.trim().is_empty() {
            continue;
        }
        let mut path: Option<PathBuf> = None;
        let mut branch: Option<String> = None;
        let mut head: Option<String> = None;
        let mut detached = false;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_string());
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                head = Some(h.to_string());
            } else if line == "detached" {
                detached = true;
            }
        }
        if let Some(path) = path {
            infos.push(WorktreeInfo {
                path,
                branch,
                head,
                detached,
                is_main: !main_seen,
            });
            main_seen = true;
        }
    }
    Ok(infos)
}

/// Directory git status. Non-git directories return is_repo=false, never error.
pub fn status(cwd: &Path) -> WorktreeStatus {
    let toplevel = match run_git(cwd, &["rev-parse", "--show-toplevel"]) {
        Ok(s) => PathBuf::from(s.trim()),
        Err(_) => {
            return WorktreeStatus {
                is_repo: false,
                branch: None,
                is_worktree: false,
                worktree_path: None,
                dirty: false,
            };
        }
    };
    let branch = run_git(cwd, &["branch", "--show-current"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let dirty = run_git(cwd, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let abs = |p: &str| {
        let pb = if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            cwd.join(p)
        };
        pb.canonicalize().ok().map(|p| plain_path(&p))
    };
    let git_dir = run_git(cwd, &["rev-parse", "--git-dir"])
        .ok()
        .and_then(|s| abs(s.trim()));
    let common_dir = run_git(cwd, &["rev-parse", "--git-common-dir"])
        .ok()
        .and_then(|s| abs(s.trim()));
    let is_worktree = matches!((git_dir, common_dir), (Some(g), Some(c)) if g != c);
    WorktreeStatus {
        is_repo: true,
        branch,
        is_worktree,
        worktree_path: Some(toplevel),
        dirty,
    }
}

/// Number of unmerged commits relative to `base`.
pub fn count_ahead(cwd: &Path, base: &str) -> Result<u64, String> {
    let out = run_git(
        cwd,
        &["rev-list", "--count", &format!("{base}..HEAD"), "--"],
    )?;
    out.trim()
        .parse()
        .map_err(|e| format!("parse commit count: {e}"))
}

/// Final cleanup decision: repo exists, working tree clean, and no unmerged commits → safe to delete.
pub fn should_remove(status: &WorktreeStatus, ahead: u64) -> bool {
    status.is_repo && !status.dirty && ahead == 0
}

/// Slugify: lowercase, non-alphanumeric to `-`, trim leading/trailing `-`, truncate to 40.
pub fn slugify(input: &str) -> String {
    let mut out = String::new();
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.starts_with('-') {
        out.remove(0);
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 40 {
        out.truncate(40);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

fn repo_lock(repo: &Path) -> std::sync::MutexGuard<'static, ()> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, &'static Mutex<()>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let lock = {
        let mut guard = map.lock().unwrap_or_else(|p| p.into_inner());
        *guard
            .entry(repo.to_path_buf())
            .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
    };
    // Per-repo locks live for the process lifetime (lightweight) so lock guards can have a 'static lifetime.
    lock.lock().unwrap_or_else(|p| p.into_inner())
}

/// Normalization validation: a worktree must live under `<repo>/.worktrees/`.
pub fn validate_worktree_dir(repo: &Path, dir: &Path) -> Result<PathBuf, String> {
    let repo = plain_path(&repo.canonicalize().map_err(|e| format!("repo path: {e}"))?);
    let base = repo.join(".worktrees");
    let p = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        repo.join(dir)
    };
    // Normalize the nearest existing ancestor (resolving symlinks like /var -> /private/var),
    // then lexically join the remainder, to support a `.worktrees/xxx` that does not exist yet.
    let mut existing = p.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if existing.exists() {
            break;
        }
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                existing = existing
                    .parent()
                    .map(|x| x.to_path_buf())
                    .unwrap_or_else(|| existing.clone());
            }
            None => break,
        }
    }
    let mut target = plain_path(
        &existing
            .canonicalize()
            .map_err(|e| format!("worktree path: {e}"))?,
    );
    for name in tail.iter().rev() {
        target.push(name);
    }
    if !target.starts_with(&base) {
        return Err(format!("worktree 必须位于 {} 内", base.display()));
    }
    Ok(target)
}

/// Create an isolated worktree for a session. Idempotent: returns the existing entry if the directory exists.
pub fn create(
    repo: &Path,
    session_id: &str,
    slug_source: &str,
    base: Option<&str>,
) -> Result<WorktreeInfo, String> {
    let repo = plain_path(&repo.canonicalize().map_err(|e| format!("repo path: {e}"))?);
    let _guard = repo_lock(&repo);
    let short_id: String = session_id.chars().take(8).collect();
    let slug = slugify(slug_source);
    let dir = repo
        .join(".worktrees")
        .join(format!("starriver-{short_id}-{slug}"));
    if dir.exists() {
        return list(&repo)?
            .into_iter()
            .find(|i| i.path == dir)
            .ok_or_else(|| format!("worktree 目录已存在但未注册: {}", dir.display()));
    }
    std::fs::create_dir_all(dir.parent().ok_or_else(|| "no parent".to_string())?)
        .map_err(|e| format!("create .worktrees: {e}"))?;
    let branch_base = format!("starriver/{short_id}-{slug}");
    let existing = run_git(&repo, &["branch", "--list", &format!("{branch_base}*")])?;
    // BUG-21: `git branch --list` prefixes the checked-out branch with `* `
    // (and branches held by other worktrees with `+ `); strip the marker so
    // the collision check actually sees them.
    let branch_exists = |name: &str| {
        existing.lines().any(|l| {
            l.trim_start()
                .trim_start_matches(['*', '+'])
                .trim()
                == name
        })
    };
    let mut branch = branch_base.clone();
    if branch_exists(&branch) {
        let mut n = 2;
        loop {
            let cand = format!("{branch_base}-{n}");
            if !branch_exists(&cand) {
                branch = cand;
                break;
            }
            n += 1;
        }
    }
    let dir_str = dir.to_string_lossy().into_owned();
    run_git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &branch,
            &dir_str,
            base.unwrap_or("HEAD"),
        ],
    )?;
    Ok(WorktreeInfo {
        path: dir,
        branch: Some(branch),
        head: None,
        detached: false,
        is_main: false,
    })
}

/// Delete a clean worktree (including its local branch). Reject when dirty or with unmerged commits relative to base.
pub fn remove(worktree_path: &Path, base: Option<&str>) -> Result<(), String> {
    let wt = worktree_path
        .canonicalize()
        .map_err(|e| format!("worktree path: {e}"))?;
    let common = run_git(&wt, &["rev-parse", "--git-common-dir"])?
        .trim()
        .to_string();
    let repo = if Path::new(&common).is_absolute() {
        PathBuf::from(&common)
    } else {
        wt.join(&common)
    };
    let repo = repo
        .canonicalize()
        .map_err(|e| format!("repo path: {e}"))?
        .parent()
        .unwrap_or(&repo)
        .to_path_buf();
    let _guard = repo_lock(&repo);
    let dirty = run_git(&wt, &["status", "--porcelain"])?;
    if !dirty.trim().is_empty() {
        return Err("worktree 有未提交改动，拒绝删除（请先提交或 stash）".into());
    }
    if let Some(base_ref) = base {
        let ahead = count_ahead(&wt, base_ref)?;
        if ahead > 0 {
            return Err(format!(
                "worktree 相对 {base_ref} 有 {ahead} 个未合并提交，拒绝删除"
            ));
        }
    }
    let branch = run_git(&wt, &["branch", "--show-current"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let wt_str = wt.to_string_lossy().into_owned();
    run_git(&repo, &["worktree", "remove", &wt_str])?;
    if let Some(b) = branch {
        let _ = run_git(&repo, &["branch", "-D", &b]); // failure is only a warning
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(args: &[&str], cwd: &Path) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        git(&["init", "-q", "-b", "main"], dir);
        git(&["config", "user.email", "t@example.com"], dir);
        git(&["config", "user.name", "test"], dir);
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        git(&["add", "-A"], dir);
        git(&["commit", "-qm", "init"], dir);
    }

    fn add_worktree(repo: &Path, name: &str, branch: &str) -> PathBuf {
        let wt = repo.join(".worktrees").join(name);
        git(
            &["worktree", "add", "-q", "-b", branch, wt.to_str().unwrap()],
            repo,
        );
        wt
    }

    #[test]
    fn list_detects_main_and_linked_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        add_worktree(tmp.path(), "wt1", "feature/x");
        let infos = list(tmp.path()).unwrap();
        assert_eq!(infos.len(), 2);
        assert!(infos[0].is_main);
        let linked = infos.iter().find(|i| !i.is_main).unwrap();
        assert_eq!(linked.branch.as_deref(), Some("feature/x"));
        assert!(linked.path.ends_with(".worktrees/wt1"));
    }

    #[test]
    fn status_non_git_dir_reports_not_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let s = status(tmp.path());
        assert!(!s.is_repo);
        assert!(s.branch.is_none());
    }

    #[test]
    fn status_returns_branch_and_clean() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let s = status(tmp.path());
        assert!(s.is_repo);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert!(!s.dirty);
        assert!(!s.is_worktree);
    }

    #[test]
    fn status_marks_worktree_and_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let wt = add_worktree(tmp.path(), "wt1", "feature/x");
        std::fs::write(wt.join("b.txt"), "b").unwrap();
        let s = status(&wt);
        assert!(s.is_repo);
        assert!(s.is_worktree);
        assert!(s.dirty);
        assert_eq!(s.branch.as_deref(), Some("feature/x"));
    }

    #[test]
    fn slugify_lowercases_and_replaces() {
        assert_eq!(slugify("Fix Login Bug!"), "fix-login-bug");
        assert_eq!(slugify(&"x".repeat(80)), "x".repeat(40));
    }

    #[test]
    fn create_makes_worktree_with_branch() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let info = create(tmp.path(), "abcd1234-xxxx", "Fix Login", None).unwrap();
        assert!(info.path.exists());
        assert_eq!(info.branch.as_deref(), Some("starriver/abcd1234-fix-login"));
        assert!(list(tmp.path()).unwrap().iter().any(|i| !i.is_main));
    }

    #[test]
    fn create_is_idempotent_when_dir_exists() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let first = create(tmp.path(), "abcd1234-xxxx", "Fix Login", None).unwrap();
        let second = create(tmp.path(), "abcd1234-xxxx", "Fix Login", None).unwrap();
        assert_eq!(first.path, second.path);
    }

    #[test]
    fn create_appends_suffix_on_branch_collision() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        git(&["branch", "starriver/abcd1234-fix-login"], tmp.path());
        let info = create(tmp.path(), "abcd1234-xxxx", "Fix Login", None).unwrap();
        assert_eq!(
            info.branch.as_deref(),
            Some("starriver/abcd1234-fix-login-2")
        );
    }

    /// BUG-21 regression: `git branch --list` prefixes the currently checked
    /// out branch with `* ` (and branches held by other worktrees with `+ `),
    /// and `str::trim` does not strip that marker — a colliding branch that
    /// is checked out escapes the collision check and `worktree add -b`
    /// fatals instead of appending the `-2` suffix.
    #[test]
    fn create_appends_suffix_when_colliding_branch_is_checked_out() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        git(
            &["checkout", "-q", "-b", "starriver/abcd1234-fix-login"],
            tmp.path(),
        );
        let info = create(tmp.path(), "abcd1234-xxxx", "Fix Login", None)
            .expect("checked-out colliding branch must still suffix, not error");
        assert_eq!(
            info.branch.as_deref(),
            Some("starriver/abcd1234-fix-login-2")
        );
    }

    #[test]
    fn remove_refuses_dirty_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let info = create(tmp.path(), "abcd1234-xxxx", "Fix Login", None).unwrap();
        std::fs::write(info.path.join("b.txt"), "b").unwrap();
        assert!(remove(&info.path, Some("main")).is_err());
        assert!(info.path.exists());
    }

    #[test]
    fn remove_removes_clean_worktree_and_branch() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let info = create(tmp.path(), "abcd1234-xxxx", "Fix Login", None).unwrap();
        remove(&info.path, Some("main")).unwrap();
        assert!(!info.path.exists());
        let branches = std::process::Command::new("git")
            .args(["branch", "--list", "starriver/abcd1234-fix-login"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&branches.stdout).trim().is_empty());
    }

    #[test]
    fn validate_rejects_escape_paths() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let outside = tempfile::tempdir().unwrap();
        assert!(validate_worktree_dir(tmp.path(), outside.path()).is_err());
        let inside = tmp.path().join(".worktrees").join("ok");
        assert!(validate_worktree_dir(tmp.path(), &inside).is_ok());
    }

    #[test]
    fn lifecycle_no_changes_removes() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let info = create(tmp.path(), "abcd1234-xxxx", "Fix Login", None).unwrap();
        let s = status(&info.path);
        let ahead = count_ahead(&info.path, "main").unwrap();
        assert!(should_remove(&s, ahead));
        remove(&info.path, Some("main")).unwrap();
        assert!(!info.path.exists());
    }

    #[test]
    fn lifecycle_with_changes_keeps() {
        let tmp = tempfile::tempdir().unwrap();
        init_repo(tmp.path());
        let info = create(tmp.path(), "abcd1234-xxxx", "Fix Login", None).unwrap();
        std::fs::write(info.path.join("b.txt"), "b").unwrap();
        let s = status(&info.path);
        assert!(!should_remove(&s, 0));
    }
    #[test]
    fn main_repo_of_resolves_from_worktree_and_main_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        // Plain drive form: `main_repo_of` strips the `\\?\` prefix that
        // Windows `canonicalize` yields (git rejects verbatim paths).
        let expected = plain_path(&repo.canonicalize().unwrap());
        // From the main tree itself.
        assert_eq!(main_repo_of(&repo).unwrap(), expected);
        // From a linked worktree created by `create`.
        let info = create(&repo, "sess1234", "task", None).unwrap();
        assert_eq!(main_repo_of(&info.path).unwrap(), expected);
    }
}
