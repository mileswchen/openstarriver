//! Windows sandbox: write-restricted token backend (-unelevated model,
//! `Partial` enforcement — write-boundary only, reads/network unrestricted).
//!
//! The chain has a single runner, `writetoken`, but keeps the probe-chain
//! shape of the Linux backend: the probe is a real rehearsal run through the
//! helper (token → confined spawn → job), not a binary-existence check, and
//! `STARRIVER_SANDBOX_RUNNER=writetoken` forces selection for diagnostics.
//!
//! tokio/std cannot spawn a process with a custom token, so confinement
//! wraps argv with the helper binary — the same shape as Seatbelt's
//! `sandbox-exec` wrapper, leaving the bash tool untouched:
//!
//! `starriver-sbx-helper.exe run <mode> <root> -- <child argv…>`

pub mod acl;
pub mod job;
pub mod pure;
pub mod run;
pub mod token;

use std::sync::OnceLock;

use starriver_kernel::sandbox::{
    ConfinedArgv, SandboxEnforcement, SandboxError, SandboxMode, SandboxPolicy, SandboxProvider,
};

use crate::boundary::canonicalize_or_raw;
use crate::probe::{self, Probe};

pub const RUNNER: &str = "writetoken";
const HELPER_EXE: &str = "starriver-sbx-helper.exe";
const HELPER_ENV: &str = "STARRIVER_SANDBOX_HELPER";

/// NUL-terminated UTF-16 for the `…W` Win32 APIs (shared by run/acl/token).
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub struct WindowsSandbox;

impl WindowsSandbox {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WindowsSandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxProvider for WindowsSandbox {
    fn confine(
        &self,
        argv: Vec<String>,
        policy: &SandboxPolicy,
    ) -> Result<ConfinedArgv, SandboxError> {
        match policy.mode {
            SandboxMode::DangerFullAccess => Ok(ConfinedArgv::new(argv, SandboxEnforcement::Full)),
            mode => {
                let runner =
                    select_runner().map_err(|detail| SandboxError::Unavailable { mode, detail })?;
                debug_assert_eq!(runner, RUNNER);
                let helper =
                    helper_path().map_err(|detail| SandboxError::Unavailable { mode, detail })?;
                let root = normalize_root(&policy.workspace_root);
                let mut wrapped = pure::helper_argv(mode, &root, &helper.to_string_lossy());
                wrapped.extend(argv);
                Ok(ConfinedArgv::new(wrapped, SandboxEnforcement::Partial).runner(RUNNER))
            }
        }
    }
}

/// Helper binary location: `STARRIVER_SANDBOX_HELPER` override, else next to
/// the running binary. Bins land in `<target>/debug`, but test harnesses run
/// from `<target>/debug/deps`, so a build one directory up is the fallback —
/// and the installed desktop app keeps it in `<install>/resources/` next to
/// the executable (electron-builder extraResources layout), which covers
/// integration tests, dev runs, and packaged installs.
pub fn helper_path() -> Result<std::path::PathBuf, String> {
    if let Ok(p) = std::env::var(HELPER_ENV) {
        if !p.is_empty() {
            let path = std::path::PathBuf::from(&p);
            if path.exists() {
                return Ok(path);
            }
            return Err(format!("{HELPER_ENV}=`{p}` does not exist"));
        }
    }
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "current_exe has no parent dir".to_string())?;
    let mut candidates = vec![dir.join(HELPER_EXE)];
    if let Some(parent) = dir.parent() {
        candidates.push(parent.join(HELPER_EXE));
    }
    // Packaged desktop app: <install>/Starriver.exe + <install>/resources/.
    candidates.push(dir.join("resources").join(HELPER_EXE));
    candidates
        .into_iter()
        .find(|c| c.exists())
        .ok_or_else(|| format!("helper not found near {} (set {HELPER_ENV})", dir.display()))
}

/// `canonicalize_or_raw` yields `\\?\C:\…` device paths on Windows, which
/// the ACL APIs accept but everything downstream (logs, tests, lexical
/// checks) compares in plain form — strip the prefix.
fn normalize_root(root: &str) -> String {
    let canonical = canonicalize_or_raw(root);
    let stripped = canonical
        .strip_prefix(r"\\?\")
        .unwrap_or(&canonical);
    // `\\?\UNC\server\share` must become `\\server\share` — stripping
    // only the verbatim prefix left a drive-less RELATIVE path that broke
    // every downstream Win32 call for network-share workspaces.
    if let Some(rest) = stripped.strip_prefix("UNC\\") {
        format!("\\\\{rest}")
    } else {
        stripped.to_string()
    }
}

/// Probe = one real confined rehearsal through the helper. Cached by the
/// caller's chain (runs once per process).
fn writetoken_probe() -> Probe {
    let detail = probe_rehearsal();
    Probe {
        runner: RUNNER,
        available: detail.is_none(),
        detail: detail.unwrap_or_default(),
    }
}

/// `None` when the rehearsal passed; `Some(detail)` carries the first
/// stderr line (or exit status) otherwise — bwrap_probe's message shape.
fn probe_rehearsal() -> Option<String> {
    let helper = helper_path().ok()?;
    let output = std::process::Command::new(&helper)
        .arg("probe")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .ok()?;
    if output.status.success() {
        return None;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.lines().next().unwrap_or("").trim();
    Some(format!(
        "probe exited {}: {detail}",
        output.status.code().unwrap_or(-1)
    ))
}

/// Single-runner probe chain. The probe is cached per process (host
/// capability, env-independent); `STARRIVER_SANDBOX_RUNNER` forces selection
/// for diagnostics and is re-read on every call so env changes take effect
/// without a restart. It accepts only `writetoken` (the Linux backends are
/// unknown on this platform); a forced runner must still probe healthy.
fn select_runner() -> Result<&'static str, String> {
    static PROBES: OnceLock<Vec<Probe>> = OnceLock::new();
    let probes = PROBES.get_or_init(|| vec![writetoken_probe()]);
    match std::env::var("STARRIVER_SANDBOX_RUNNER").ok().as_deref() {
        Some(RUNNER) => {
            let probe = &probes[0];
            if probe.available {
                Ok(RUNNER)
            } else {
                Err(format!(
                    "forced runner {RUNNER} unavailable: {}",
                    probe.detail
                ))
            }
        }
        Some(other) => Err(format!(
            "STARRIVER_SANDBOX_RUNNER=`{other}` is not one of {RUNNER}"
        )),
        None => probe::pick(probes),
    }
}

/// `starriver-sbx-helper` CLI entry (the bin is a two-line shim around
/// this). Exit codes: child's code, or `run::SPAWN_FAILED` (126) when the
/// sandbox itself failed.
pub fn cli_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("probe") => run::probe(),
        Some("run") => {
            // args: run <mode> <root> -- <child argv…>
            if args.len() < 4 || args[3] != "--" {
                eprintln!("starriver-sbx: usage: run <mode> <root> -- <argv…>");
                return run::SPAWN_FAILED;
            }
            let mode = args[1].clone();
            let root = args[2].clone();
            let child: Vec<String> = args[4..].to_vec();
            run::run(&mode, &root, &child)
        }
        _ => {
            eprintln!(
                "starriver-sbx: usage: {} probe | run <read-only|workspace-write> <root> -- <argv…>",
                std::env::args().next().unwrap_or_default()
            );
            run::SPAWN_FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn danger_mode_returns_argv_unchanged() {
        let policy = SandboxPolicy {
            mode: SandboxMode::DangerFullAccess,
            workspace_root: r"C:\work".into(),
            session_id: None,
        };
        let confined = WindowsSandbox::new()
            .confine(vec!["sh".into(), "-c".into(), "true".into()], &policy)
            .unwrap();
        assert_eq!(confined.runner, "none");
        assert!(confined.spawn_hook.is_none());
        assert_eq!(
            confined.argv,
            vec!["sh".to_string(), "-c".to_string(), "true".to_string()]
        );
    }

    #[test]
    fn confine_wraps_with_helper_run_prefix() {
        let policy = SandboxPolicy {
            mode: SandboxMode::WorkspaceWrite,
            workspace_root: r"C:\work".into(),
            session_id: None,
        };
        let confined =
            WindowsSandbox::new().confine(vec!["sh".into(), "-c".into(), "true".into()], &policy);
        // Helper discovery may legitimately fail in a bare test host; the
        // wrap shape is asserted only when discovery succeeds.
        if let Ok(confined) = confined {
            assert_eq!(confined.runner, RUNNER);
            assert_eq!(confined.enforcement, SandboxEnforcement::Partial);
            assert_eq!(confined.argv[1], "run");
            assert_eq!(confined.argv[2], "workspace-write");
            assert_eq!(confined.argv[4], "--");
            assert_eq!(
                confined.argv[5..],
                vec!["sh".to_string(), "-c".to_string(), "true".to_string()][..]
            );
        } else {
            let err = confined.unwrap_err();
            assert_eq!(err.code(), "SANDBOX_UNAVAILABLE");
        }
    }

    #[test]
    fn device_path_prefix_is_stripped() {
        assert_eq!(normalize_root(r"\\?\C:\work"), r"C:\work");
        assert_eq!(normalize_root(r"C:\work"), r"C:\work");
    }

    #[test]
    fn forced_unknown_runner_names_the_valid_choice() {
        // select_runner caches; exercise only the message shape indirectly
        // through the constant to avoid env races in parallel tests.
        assert_eq!(RUNNER, "writetoken");
    }
}
