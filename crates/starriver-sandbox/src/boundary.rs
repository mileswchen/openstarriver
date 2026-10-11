//! Workspace write boundary — cross-platform pure logic shared by every
//! backend (and by the non-bash write tools, which cannot be confined by an
//! exec wrapper and are checked lexically instead).

use std::path::Path;

use starriver_kernel::sandbox::{SandboxError, SandboxMode};

/// Canonicalize a path for subpath matching, falling back to the raw string
/// when the path does not exist. `~/` expands against `$HOME` first.
pub fn canonicalize_or_raw(p: &str) -> String {
    let expanded = if let Some(rest) = p.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            p.to_string()
        } else {
            format!("{}/{}", home.trim_end_matches('/'), rest)
        }
    } else {
        p.to_string()
    };
    Path::new(&expanded)
        .canonicalize()
        .map(|c| c.to_string_lossy().to_string())
        .unwrap_or(expanded)
}

/// The sandbox writable root for an execution working directory. Worktrees
/// live under `<workspace>/.worktrees/…` (single source:
/// `starriver_tools::worktree::create`), so a session running inside one
/// keeps its write boundary at the workspace itself — commits and object
/// writes into the main repo stay legal while everything else remains
/// denied. Plain sessions keep their cwd as the root.
pub fn effective_workspace_root(cwd: &Path) -> std::path::PathBuf {
    for ancestor in cwd.ancestors() {
        if ancestor.file_name() == Some(std::ffi::OsStr::new(".worktrees")) {
            if let Some(workspace) = ancestor.parent() {
                return workspace.to_path_buf();
            }
        }
    }
    cwd.to_path_buf()
}

/// Writable roots outside the workspace for the lexical check: POSIX
/// whitelists `/tmp` and its canonical `/private/tmp` form (same set the
/// seatbelt profile allows); Windows whitelists `%TEMP%` — the same pair of
/// roots the writetoken backend grants. Windows paths are case-insensitive
/// and compared on a component boundary (lowercased string prefix with a
/// trailing-separator check).
#[cfg(target_os = "windows")]
fn within_boundary(absolute: &Path, root: &Path) -> bool {
    // Compare on a single separator and case form: forward slashes are legal
    // in Windows paths, and `env::temp_dir()` (GetTempPathW) keeps a trailing
    // backslash, which would otherwise defeat the boundary check below.
    let norm = |p: &Path| {
        p.to_string_lossy()
            .to_ascii_lowercase()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_string()
    };
    let target = norm(absolute);
    let temp = std::env::temp_dir();
    [norm(root), norm(&temp)]
        .into_iter()
        .any(|base| target == base || target.starts_with(&format!("{base}\\")))
}

#[cfg(not(target_os = "windows"))]
fn within_boundary(absolute: &Path, root: &Path) -> bool {
    absolute.starts_with(root)
        || absolute.starts_with("/tmp")
        || absolute.starts_with("/private/tmp")
}

/// P0 boundary enforcement: under workspace-write, file write targets must be inside
/// the workspace root (or /tmp) — same semantics as the bash sandbox whitelist,
/// closing the gap where edit/write/patch bypass the exec sandbox and write arbitrary
/// absolute paths directly. Read-only mode is rejected separately (deny_in_readonly);
/// danger-full-access does not go through this check.
pub fn ensure_workspace_writable(
    path: &Path,
    mode: Option<SandboxMode>,
    cwd: Option<&Path>,
) -> Result<(), SandboxError> {
    match mode {
        Some(SandboxMode::WorkspaceWrite) => {}
        _ => return Ok(()),
    }
    // No session cwd = no workspace boundary to guard (bare session/test):
    // lexical checking is meaningless, so allow. With a cwd it is the only write
    // boundary — relative paths are anchored to the root per POSIX semantics before
    // the check (resolve_path keeps the relative form when cwd is absent, so a plain
    // starts_with against an absolute root would always be false).
    let Some(session_cwd) = cwd else {
        return Ok(());
    };
    let root = effective_workspace_root(session_cwd);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    // Lexical containment check + `..` escape interception
    let escapes = absolute
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir));
    let contained = within_boundary(&absolute, &root);
    if !escapes && contained {
        Ok(())
    } else {
        Err(SandboxError::Confine(format!(
            "write target `{}` is outside the workspace root `{}`",
            absolute.display(),
            root.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_write_relative_paths_anchor_to_cwd() {
        let cwd = std::path::Path::new("/work/proj");
        assert!(ensure_workspace_writable(
            std::path::Path::new("/work/proj/out.txt"),
            Some(SandboxMode::WorkspaceWrite),
            Some(cwd),
        )
        .is_ok());
        // Relative paths are allowed after anchoring to the root
        assert!(ensure_workspace_writable(
            std::path::Path::new("out.txt"),
            Some(SandboxMode::WorkspaceWrite),
            Some(cwd),
        )
        .is_ok());
        assert!(ensure_workspace_writable(
            std::path::Path::new("/etc/hosts"),
            Some(SandboxMode::WorkspaceWrite),
            Some(cwd),
        )
        .is_err());
        // `..` escape rejected
        assert!(ensure_workspace_writable(
            std::path::Path::new("../escape.txt"),
            Some(SandboxMode::WorkspaceWrite),
            Some(cwd),
        )
        .is_err());
    }

    #[test]
    fn workspace_write_without_cwd_has_no_boundary() {
        // Missing cwd = no workspace boundary to guard → allow (both relative
        // and absolute paths; the old implementation fell back to a literal `/`
        // root, so relative paths could never pass the lexical check).
        assert!(ensure_workspace_writable(
            std::path::Path::new("./out.txt"),
            Some(SandboxMode::WorkspaceWrite),
            None,
        )
        .is_ok());
        assert!(ensure_workspace_writable(
            std::path::Path::new("/tmp/ok.txt"),
            Some(SandboxMode::WorkspaceWrite),
            None,
        )
        .is_ok());
        assert!(ensure_workspace_writable(
            std::path::Path::new("/any/where.txt"),
            Some(SandboxMode::WorkspaceWrite),
            None,
        )
        .is_ok());
    }

    #[test]
    fn worktree_cwd_lifts_boundary_to_workspace() {
        let cwd = std::path::Path::new("/work/.worktrees/fix-1/src");
        assert_eq!(effective_workspace_root(cwd), std::path::Path::new("/work"));
        assert_eq!(
            effective_workspace_root(std::path::Path::new("/work/proj")),
            std::path::Path::new("/work/proj")
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_boundary_allows_workspace_and_temp_denies_rest() {
        let temp = std::env::temp_dir();
        let cwd = std::path::Path::new(r"C:\work\proj");
        let mode = Some(SandboxMode::WorkspaceWrite);
        assert!(ensure_workspace_writable(
            std::path::Path::new(r"C:\work\proj\out.txt"),
            mode,
            Some(cwd),
        )
        .is_ok());
        // Case-insensitive on Windows.
        assert!(ensure_workspace_writable(
            std::path::Path::new(r"C:\WORK\PROJ\out.txt"),
            mode,
            Some(cwd),
        )
        .is_ok());
        // %TEMP% is whitelisted (writetoken grants the same root).
        assert!(ensure_workspace_writable(&temp.join("x.txt"), mode, Some(cwd)).is_ok());
        // No sibling-prefix false positive: c:\work\proj2 is OUTSIDE c:\work\proj.
        assert!(ensure_workspace_writable(
            std::path::Path::new(r"C:\work\proj2\out.txt"),
            mode,
            Some(cwd),
        )
        .is_err());
        assert!(ensure_workspace_writable(
            std::path::Path::new(r"C:\Windows\System32\evil.dll"),
            mode,
            Some(cwd),
        )
        .is_err());
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn posix_boundary_whitelists_tmp_ignores_workspace_root() {
        // The lexical check whitelists /tmp (and its canonical /private/tmp
        // form on macOS) regardless of the workspace root — the same roots
        // the seatbelt profile allows.
        let cwd = std::path::Path::new("/work/proj");
        let mode = Some(SandboxMode::WorkspaceWrite);
        assert!(
            ensure_workspace_writable(std::path::Path::new("/tmp/x.txt"), mode, Some(cwd)).is_ok()
        );
        assert!(ensure_workspace_writable(
            std::path::Path::new("/private/tmp/x.txt"),
            mode,
            Some(cwd)
        )
        .is_ok());
        // /tmp-prefixed lookalikes are NOT the temp root.
        assert!(ensure_workspace_writable(
            std::path::Path::new("/tmpx/escape.txt"),
            mode,
            Some(cwd),
        )
        .is_err());
        // Component-boundary matching: /work/proj2 is a sibling, not a child.
        assert!(ensure_workspace_writable(
            std::path::Path::new("/work/proj2/out.txt"),
            mode,
            Some(cwd),
        )
        .is_err());
    }

    #[test]
    fn canonicalize_or_raw_expands_home_or_keeps_raw() {
        // Serializes HOME access: other tests in the crate read env vars, and
        // libtest runs tests in threads within one process.
        static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // Existing path → canonical form (e.g. /tmp → /private/tmp on macOS).
        let dir = std::env::temp_dir().join(format!("sb-boundary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let canon = canonicalize_or_raw(dir.to_str().unwrap());
        assert_eq!(
            canon,
            dir.canonicalize().unwrap().to_str().unwrap().to_string()
        );
        let _ = std::fs::remove_dir(&dir);

        // Nonexistent path stays raw (no Err, no canonicalization).
        assert_eq!(
            canonicalize_or_raw("/definitely/not/here/zzz"),
            "/definitely/not/here/zzz"
        );

        // ~/ expands against $HOME; empty HOME keeps the raw form.
        let saved = std::env::var("HOME").ok();
        std::env::set_var("HOME", "/home/tester");
        assert_eq!(
            canonicalize_or_raw("~/docs/a.txt"),
            "/home/tester/docs/a.txt"
        );
        // Trailing slashes on HOME don't double up.
        std::env::set_var("HOME", "/home/tester/");
        assert_eq!(
            canonicalize_or_raw("~/docs/a.txt"),
            "/home/tester/docs/a.txt"
        );
        std::env::set_var("HOME", "");
        assert_eq!(canonicalize_or_raw("~/docs/a.txt"), "~/docs/a.txt");
        match saved {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    /// The lexical check is deliberately symlink-blind (confinement is the
    /// exec sandbox's job); pin that so a future "fix" doesn't silently
    /// change the security model without this test being revisited.
    #[test]
    fn lexical_boundary_does_not_resolve_symlinks() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, cwd.join("link.txt")).unwrap();
        // The symlink target lives outside the workspace, but the lexical
        // check only sees the in-bounds path — allowed by design.
        assert!(ensure_workspace_writable(
            std::path::Path::new("link.txt"),
            Some(SandboxMode::WorkspaceWrite),
            Some(&cwd),
        )
        .is_ok());
    }
}
