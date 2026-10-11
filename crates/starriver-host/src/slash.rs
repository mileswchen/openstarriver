//! Host capabilities behind TUI slash commands: /compact /diff /usage /init /export.
//!
//! Stateless logic is split into pure functions for testability; methods that need the session store
//! bridge synchronously via `block_on` on [`HostRuntime::runtime_handle`] (same pattern as `shutdown`).

use std::path::{Path, PathBuf};
use std::process::Command as Proc;

use anyhow::{Context, Result};
use starriver_kernel::types::Message;

use crate::export;

/// Read the workspace git diff (stat + body + brief status). Non-git repos return an error.
pub fn workspace_diff(cwd: &str) -> Result<String> {
    let is_repo = Proc::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .context("git unavailable")?
        .status
        .success();
    if !is_repo {
        anyhow::bail!("not a git repository: {cwd}");
    }
    let status = run_git(cwd, &["status", "--short"])?;
    // HEAD diff covers staged + unstaged — matching workspace_diff_stat's
    // `--numstat HEAD`. Plain `git diff` misses staged-only changes, so a
    // fully-staged edit showed in status but produced an empty diff body.
    let stat = run_git(cwd, &["diff", "--stat", "HEAD"])?;
    let body = run_git(cwd, &["diff", "HEAD"])?;
    if status.trim().is_empty() && stat.trim().is_empty() && body.trim().is_empty() {
        return Ok(String::new());
    }
    Ok(format!("{status}\n{stat}\n{body}"))
}

/// Structured per-file change counts for the mobile diff pill: `git diff
/// --numstat` + branch, from the working tree (unstaged + staged, untracked
/// files counted with both sides 0 like `--stat`'s summary convention).
///
/// Submodule-aware: from a superproject directory a gitlink change numstats
/// as a single `+1 -1` pointer row — the phone's pill then reads "nothing
/// changed" while the real edits live inside the submodule (exactly the
/// products/iphone → StarriverMobile case). Initialized submodules are
/// recursed into (depth 2) and their per-file stats merged in place of the
/// pointer rows.
pub fn workspace_diff_stat(cwd: &str) -> Result<DiffStat> {
    workspace_diff_stat_depth(cwd, 0)
}

fn workspace_diff_stat_depth(cwd: &str, depth: u32) -> Result<DiffStat> {
    let is_repo = Proc::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .context("git unavailable")?
        .status
        .success();
    if !is_repo {
        anyhow::bail!("not a git repository: {cwd}");
    }
    let branch = run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string();
    // `--relative` (implicit cwd): numstat paths come out relative to `cwd`,
    // matching `git ls-files -s` — required for the gitlink merge below, and
    // it scopes the pill to the workspace directory (the working semantic).
    // NOTE: git runs with `current_dir`, NOT `git -C`: `-C` combined with
    // `--relative` silently yields EMPTY output (prefix resolution happens
    // before -C applies), which would read as "no changes" forever.
    let numstat = Proc::new("git")
        .current_dir(cwd)
        .args(["diff", "--numstat", "--relative", "HEAD"])
        .output()
        .context("git unavailable")?;
    let numstat = if numstat.status.success() {
        String::from_utf8_lossy(&numstat.stdout).into_owned()
    } else {
        // Only an unborn branch (no HEAD) legitimately answers "diff HEAD"
        // with a failure — everything else must surface, not silently read
        // as "no changes".
        let stderr = String::from_utf8_lossy(&numstat.stderr).into_owned();
        if stderr.contains("ambiguous argument 'HEAD'")
            || stderr.contains("unknown revision")
            || stderr.contains("bad revision")
        {
            String::new()
        } else {
            anyhow::bail!("git diff --numstat failed in {cwd}: {stderr}");
        }
    };
    let mut files = Vec::new();
    for line in numstat.lines() {
        // Format: `<add>\t<del>\t<path>`; binary files render `-\t-\t<path>`.
        let mut parts = line.splitn(3, '\t');
        let (Some(add), Some(del), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let parse = |v: &str| v.parse::<u64>().unwrap_or(0);
        files.push(DiffFile {
            path: path.to_string(),
            additions: parse(add),
            deletions: parse(del),
        });
    }
    // Untracked files: visible changes but no numstat row yet.
    let status = run_git(cwd, &["status", "--short"])?;
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("?? ") else {
            continue;
        };
        let path = rest.trim().to_string();
        if !files.iter().any(|f| f.path == path) {
            files.push(DiffFile {
                path,
                additions: 0,
                deletions: 0,
            });
        }
    }
    // Submodule-aware merge. Two real-world shapes must both surface:
    // ① moved pointer (numstat row +1 −1) → replace the pointer row with the
    //    submodule's own diff when it has one, keep the row when clean;
    // ② dirty submodule with an UNMOVED pointer → no numstat row exists at
    //    all, yet the inner edits are real workspace changes → append them.
    if depth < 2 {
        let gitlinks = gitlink_paths(cwd);
        if !gitlinks.is_empty() {
            let mut merged: Vec<DiffFile> = Vec::with_capacity(files.len() + 4);
            for file in files {
                if gitlinks.iter().any(|g| g == &file.path) {
                    match workspace_diff_stat_depth(&format!("{cwd}/{}", file.path), depth + 1) {
                        Ok(sub_stat) if !sub_stat.files.is_empty() => {
                            for mut sub_file in sub_stat.files {
                                sub_file.path = format!("{}/{}", file.path, sub_file.path);
                                merged.push(sub_file);
                            }
                        }
                        // Uninitialized/unreadable submodule or clean inner
                        // tree: the pointer move is the only visible change.
                        _ => merged.push(file),
                    }
                } else {
                    merged.push(file);
                }
            }
            for g in &gitlinks {
                let prefix = format!("{g}/");
                if merged.iter().any(|f| f.path.starts_with(&prefix)) {
                    continue;
                }
                if let Ok(sub_stat) = workspace_diff_stat_depth(&format!("{cwd}/{g}"), depth + 1) {
                    for mut sub_file in sub_stat.files {
                        sub_file.path = format!("{prefix}{}", sub_file.path);
                        merged.push(sub_file);
                    }
                }
            }
            files = merged;
        }
    }
    let totals = files
        .iter()
        .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
    Ok(DiffStat {
        branch,
        files,
        total_additions: totals.0,
        total_deletions: totals.1,
    })
}

/// Staged entries with mode 160000 are gitlinks (submodule pointers).
fn gitlink_paths(cwd: &str) -> Vec<String> {
    let Ok(out) = run_git(cwd, &["ls-files", "-s"]) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let meta = parts.next()?;
            let path = parts.next()?.trim().to_string();
            (meta.split_whitespace().next() == Some("160000")).then_some(path)
        })
        .collect()
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DiffStat {
    pub branch: String,
    pub files: Vec<DiffFile>,
    pub total_additions: u64,
    pub total_deletions: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DiffFile {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

fn run_git(cwd: &str, args: &[&str]) -> Result<String> {
    let out = Proc::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .context("git unavailable")?;
    if !out.status.success() {
        // An unborn branch (no HEAD) legitimately answers "diff HEAD" with a
        // failure and empty output — keep the empty-diff behavior there, but
        // surface real errors instead of masking them as "no changes".
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.contains("ambiguous argument 'HEAD'") {
            anyhow::bail!("git {:?} failed: {}", args, stderr.trim());
        }
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Generate an AGENTS.md for the project (does not overwrite if present), returning its path.
pub fn init_agents_md(cwd: &str) -> Result<PathBuf> {
    let dir = Path::new(cwd);
    if !dir.is_dir() {
        anyhow::bail!("directory does not exist: {cwd}");
    }
    let path = dir.join("AGENTS.md");
    if !path.exists() {
        std::fs::write(&path, export::AGENTS_MD_TEMPLATE)?;
    }
    Ok(path)
}

/// Export session messages to a Markdown file, returning the written path.
pub fn export_session_md(session_id: &str, cwd: &str, messages: &[Message]) -> Result<PathBuf> {
    let dir = Path::new(cwd);
    if !dir.is_dir() {
        anyhow::bail!("directory does not exist: {cwd}");
    }
    let md = export::render_session_markdown(messages);
    let path = dir.join(format!("starriver-export-{session_id}.md"));
    std::fs::write(&path, md)?;
    Ok(path)
}

impl crate::runtime::HostRuntime {
    /// Structured diff summary for remote clients (mobile diff pill).
    pub fn workspace_diff_stat(&self, cwd: &str) -> Result<DiffStat> {
        workspace_diff_stat(cwd)
    }

    /// /usage: estimate session token usage and message count.
    pub fn session_usage(&self, session_id: &str) -> Result<(usize, usize)> {
        let inner = self.inner.clone();
        let sid = session_id.to_string();
        self.runtime_handle.block_on(async move {
            let l = inner.lock().await;
            let runtime = l
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("runtime not ready"))?;
            // A2a closeout: history comes from the coordinator log — the
            // legacy store never received post-migration messages.
            let msgs = crate::sessions::load_model_messages(runtime, &sid)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let tokens = starriver_session::compaction::estimate_messages_tokens(&msgs);
            Ok((tokens, msgs.len()))
        })
    }

    /// /diff: git diff of the current project workspace.
    pub fn workspace_diff(&self, cwd: &str) -> Result<String> {
        workspace_diff(cwd)
    }

    /// /init: generate an AGENTS.md for the current project.
    pub fn init_project_agents(&self, cwd: &str) -> Result<PathBuf> {
        init_agents_md(cwd)
    }

    /// /export: export a session to a Markdown file.
    pub fn export_session(&self, session_id: &str, cwd: &str) -> Result<PathBuf> {
        let inner = self.inner.clone();
        let sid = session_id.to_string();
        let msgs = self.runtime_handle.block_on(async move {
            let l = inner.lock().await;
            let runtime = l
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("runtime not ready"))?;
            crate::sessions::load_model_messages(runtime, &sid)
                .await
                .map_err(|e: anyhow::Error| anyhow::anyhow!("{e}"))
        })?;
        export_session_md(session_id, cwd, &msgs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_agents_md_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy().into_owned();
        let p1 = init_agents_md(&cwd).unwrap();
        assert!(p1.ends_with("AGENTS.md"));
        assert!(p1.exists());
        // Second call does not overwrite
        std::fs::write(&p1, "custom").unwrap();
        let p2 = init_agents_md(&cwd).unwrap();
        assert_eq!(std::fs::read_to_string(&p2).unwrap(), "custom");
    }

    #[test]
    fn init_missing_dir_errors() {
        assert!(init_agents_md("/nonexistent/starriver-test-xyz").is_err());
    }

    #[test]
    fn export_writes_markdown_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy().into_owned();
        let msgs = vec![Message::user("hi"), Message::assistant("yo")];
        let path = export_session_md("s1", &cwd, &msgs).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("## user"));
        assert!(content.contains("## assistant"));
    }

    #[test]
    fn diff_not_a_repo_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_string_lossy().into_owned();
        assert!(workspace_diff(&cwd).is_err());
    }

    #[test]
    fn diff_empty_repo_returns_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        Proc::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["init", "-q"])
            .output()
            .expect("git init");
        // Empty repo with no changes → empty string
        assert_eq!(workspace_diff(&cwd.to_string_lossy()).unwrap(), "");
    }
}

#[cfg(test)]
mod diff_stat_tests {
    use super::*;

    #[test]
    fn numstat_lines_parse_into_files() {
        // Pure parse check via the public fn on a real temp repo.
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let git = |args: &[&str]| {
            Proc::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        std::fs::write(cwd.join("base.txt"), "base\n").unwrap();
        // a.rs enters HEAD at two lines, then the worktree shrinks it to one:
        // numstat HEAD must report +1 −2 for the modification.
        std::fs::write(cwd.join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        git(&["add", "-A"]);
        let commit = git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "init",
        ]);
        assert!(
            commit.status.success(),
            "initial commit must succeed: {}",
            String::from_utf8_lossy(&commit.stderr)
        );
        std::fs::write(cwd.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(cwd.join("new.txt"), "untracked\n").unwrap();

        let stat = workspace_diff_stat(cwd.to_str().unwrap()).unwrap();
        // Branch reporting is git's business (master/main/HEAD on detached);
        // we only pin that the field is populated.
        assert!(!stat.branch.is_empty(), "branch populated");
        let a = stat.files.iter().find(|f| f.path == "a.rs").expect("a.rs");
        assert_eq!(
            (a.additions, a.deletions),
            (0, 1),
            "2 lines -> 1 line is +0 −1"
        );
        let newf = stat
            .files
            .iter()
            .find(|f| f.path == "new.txt")
            .expect("untracked listed");
        assert_eq!((newf.additions, newf.deletions), (0, 0));
        assert_eq!(stat.total_additions, 0);
        assert_eq!(stat.total_deletions, 1);
    }

    #[test]
    fn non_repo_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(workspace_diff_stat(dir.path().to_str().unwrap()).is_err());
    }

    /// Regression (diff pill "+1 -1 never updates"): a workspace whose real edits
    /// live in a submodule (products/iphone → StarriverMobile). While the
    /// submodule is dirty with an unmoved pointer, plain numstat shows
    /// NOTHING for it — the pill then never changes. The inner edits must be
    /// merged in, keyed by the staged gitlink list.
    #[test]
    fn submodule_pointer_row_is_replaced_by_inner_diff() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git_at = |cwd: &std::path::Path, args: &[&str]| {
            Proc::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .output()
                .unwrap()
        };
        // Submodule repo (clone SOURCE): one init commit. The later dirty
        // edit must land inside the CLONE (sup/sub) — `submodule add` clones,
        // it does not link the source dir; this mirrors the real workspace
        // where edits happen directly in the checked-out submodule.
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        git_at(&sub, &["init", "-q", "--initial-branch=main"]);
        std::fs::write(sub.join("inner.txt"), "one\ntwo\n").unwrap();
        git_at(&sub, &["add", "-A"]);
        assert!(git_at(&sub, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"])
            .status
            .success());

        // Superproject: base commit + register the submodule (pointer pinned
        // at the sub's init commit).
        let sup = root.join("sup");
        std::fs::create_dir(&sup).unwrap();
        git_at(&sup, &["init", "-q", "--initial-branch=main"]);
        std::fs::write(sup.join("readme.md"), "r\n").unwrap();
        git_at(&sup, &["add", "-A"]);
        assert!(git_at(&sup, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"])
            .status
            .success());
        let sub_url = format!("{}", sub.display());
        let added = git_at(&sup, &["-c", "protocol.file.allow=always", "submodule", "add", "-q", &sub_url, "sub"]);
        assert!(
            added.status.success(),
            "submodule add must succeed: {}",
            String::from_utf8_lossy(&added.stderr)
        );
        assert!(git_at(&sup, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "add sub"])
            .status
            .success());
        // The actual work: an UNCOMMITTED edit inside the checked-out clone,
        // with the superproject's pointer left unmoved — invisible to plain
        // numstat, exactly the pill-never-updates shape.
        std::fs::write(sup.join("sub/inner.txt"), "ONE\ntwo\nthree\nfour\n").unwrap();

        let stat = workspace_diff_stat(sup.to_str().unwrap()).unwrap();
        let inner = stat
            .files
            .iter()
            .find(|f| f.path == "sub/inner.txt")
            .expect("dirty submodule's inner file merged into the stat");
        assert_eq!((inner.additions, inner.deletions), (3, 1));
        assert!(stat.files.iter().all(|f| f.path != "sub"), "no pointer noise");
        assert_eq!(stat.total_additions, 3);
        assert_eq!(stat.total_deletions, 1);
    }
}
