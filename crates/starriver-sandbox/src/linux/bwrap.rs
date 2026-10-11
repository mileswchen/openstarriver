//! bubblewrap runner: bind-mount confinement (Full enforcement).
//!
//! `/` is re-mounted read-only, then workspace root + `/tmp` (+ the session
//! runtime dir, whose sockets need write-on-inode to connect) are layered
//! read-write on top — seatbelt's allow/deny semantics on Linux. The pid
//! namespace is unshared so bubblewrap is PID 1: when it dies (kill_on_drop,
//! cancel, timeout) the kernel reaps the whole remaining process tree,
//! closing the cancel-orphan leak noted in design/33 §7.4.

use starriver_kernel::sandbox::{ConfinedArgv, SandboxEnforcement, SandboxError, SandboxMode};

use crate::boundary::canonicalize_or_raw;

/// Probe bubblewrap with a near-real rehearsal of the confinement flags —
/// exercises unprivileged userns + mount namespace creation (what AppArmor /
/// hardened-procfs gates deny), not just binary presence. Cached by the
/// caller's probe chain.
pub fn available() -> (bool, String) {
    let output = std::process::Command::new("bwrap")
        .args([
            "--unshare-pid",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--ro-bind",
            "/",
            "/",
            "--",
            "true",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();
    match output {
        Ok(out) if out.status.success() => (true, String::new()),
        Ok(out) => {
            let detail = String::from_utf8_lossy(&out.stderr);
            let detail = detail.lines().next().unwrap_or("").trim();
            (
                false,
                format!("probe exited {}: {detail}", out.status.code().unwrap_or(-1)),
            )
        }
        Err(e) => (false, format!("not runnable: {e}")),
    }
}

/// Pure argv template for one confined mode, in flag order (later binds
/// override earlier ones). `xdg_runtime_dir` is passed in rather than read
/// from the environment to keep the function testable.
pub fn argv_for(
    mode: SandboxMode,
    workspace_root: &str,
    xdg_runtime_dir: Option<&str>,
) -> Vec<String> {
    let root = canonicalize_or_raw(workspace_root);
    let mut argv: Vec<String> = [
        "bwrap",
        "--new-session",
        "--die-with-parent",
        "--unshare-pid",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    if mode == SandboxMode::WorkspaceWrite {
        argv.extend([
            "--bind".into(),
            root.clone(),
            root,
            // Read-write /tmp mirrors the seatbelt whitelist (and the lexical
            // boundary check for non-bash tools).
            "--bind-try".into(),
            "/tmp".into(),
            "/tmp".into(),
        ]);
        // Session bus / agent / display sockets live under XDG_RUNTIME_DIR;
        // connecting to a unix socket requires write on the socket inode, so
        // ro / would cut dbus/X11/ssh-agent — seatbelt allows them, so bind
        // the runtime dir rw for parity. `--bind-try`: hosts without a user
        // session dir keep working.
        if let Some(dir) = xdg_runtime_dir.filter(|d| {
            // Only the real per-user runtime dir gets a rw re-bind — an
            // inherited XDG_RUNTIME_DIR=$HOME (or "/") would silently widen
            // the writable set far past {workspace, /tmp}.
            !d.is_empty() && d.starts_with("/run/user/")
        }) {
            argv.extend(["--bind-try".into(), dir.into(), dir.into()]);
        }
    }
    argv.push("--".into());
    argv
}

pub fn confine(
    argv: Vec<String>,
    mode: SandboxMode,
    workspace_root: &str,
) -> Result<ConfinedArgv, SandboxError> {
    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    let mut wrapped = argv_for(mode, workspace_root, xdg.as_deref());
    wrapped.extend(argv);
    Ok(ConfinedArgv::new(wrapped, SandboxEnforcement::Full).runner("bwrap"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(argv: &[&str]) -> Vec<String> {
        argv.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn workspace_write_layers_rw_over_ro_root() {
        let argv = argv_for(SandboxMode::WorkspaceWrite, "/work", Some("/run/user/9"));
        let joined = argv.join(" ");
        // Order matters: ro / first, rw rebinds after (later wins).
        let ro = argv.iter().position(|a| a == "--ro-bind").unwrap();
        let rw = argv.iter().position(|a| a == "--bind").unwrap();
        assert!(ro < rw, "rw binds must come after ro /: {joined}");
        assert!(joined.contains("--bind /work /work"));
        assert!(joined.contains("--bind-try /tmp /tmp"));
        assert!(joined.contains("--bind-try /run/user/9 /run/user/9"));
        assert!(joined.ends_with("--"));
        assert!(joined.contains("--unshare-pid"));
        assert!(joined.contains("--die-with-parent"));
    }

    #[test]
    fn read_only_has_no_rw_layers() {
        let argv = argv_for(SandboxMode::ReadOnly, "/work", Some("/run/user/9"));
        let joined = argv.join(" ");
        assert!(!joined.contains("--bind /work"));
        assert!(!joined.contains("--bind-try /tmp"));
        // XDG parity layer is workspace-write-only: read-only sessions deny
        // socket-connecting writes just like seatbelt's `(deny file-write*)`.
        assert!(!joined.contains("/run/user/9"));
        assert_eq!(
            argv,
            plain(&[
                "bwrap",
                "--new-session",
                "--die-with-parent",
                "--unshare-pid",
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--",
            ])
        );
    }

    #[test]
    fn empty_xdg_dir_is_skipped() {
        let argv = argv_for(SandboxMode::WorkspaceWrite, "/work", Some(""));
        assert!(!argv.join(" ").contains("/run/user"));
    }
}
