//! macOS Seatbelt backend: wraps argv with `sandbox-exec -p <profile> --`.
//!
//! Profile rules (verified on macOS hosts):
//! `(allow default)` is required (even execvp is denied without it); deny
//! rules veto regardless of order, so specific allows are expressed as a
//! single deny rule with `require-not` / `require-any`; path matching happens
//! on canonical paths (`/tmp` is `/private/tmp`).

use starriver_kernel::sandbox::{
    ConfinedArgv, SandboxEnforcement, SandboxError, SandboxMode, SandboxPolicy, SandboxProvider,
};

use crate::boundary::canonicalize_or_raw;

/// BUG-101: deny the disk-image / disk-management XPC services inside the
/// sandbox. `hdiutil` and friends talk to these root daemons via XPC; under
/// seatbelt the request FREEZES (never fails, never returns) which hangs the
/// whole tool call. An explicit deny makes the lookup fail fast with EPERM
/// instead, so the command errors out immediately (classified as
/// SANDBOX_DENIED by `denial::classify`).
const XPC_DISK_SERVICE_DENIES: &str = "(deny mach-lookup (xpc-service-name \"com.apple.diskimagesiod\"))\n\
(deny mach-lookup (global-name \"com.apple.diskimagesiod\"))\n\
(deny mach-lookup (global-name \"com.apple.diskmanagementd\"))\n";

/// Seatbelt `(version 1)` profile text for one confined mode.
fn seatbelt_profile(mode: SandboxMode, workspace_root: &str) -> String {
    match mode {
        SandboxMode::ReadOnly => {
            // Reads and exec allowed; every file write denied. `(deny
            // file-write*)` alone covers file-write-data and friends — the
            // `file-write-data*` variant is an unbound seatbelt variable that
            // makes sandbox-exec reject the whole profile (every bash call
            // fails with "unbound variable: file-write-data*").
            let mut profile = "(version 1)\n(allow default)\n(deny file-write*)\n".to_string();
            profile.push_str(XPC_DISK_SERVICE_DENIES);
            profile
        }
        SandboxMode::WorkspaceWrite => {
            // Workspace + tmp writable; everything else's writes denied.
            let mut allowed: Vec<String> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for w in [workspace_root.to_string(), "/tmp".to_string()] {
                let cw = canonicalize_or_raw(&w);
                if seen.insert(cw.clone()) {
                    allowed.push(cw);
                }
            }
            let subpaths = allowed
                .iter()
                .map(|a| format!("(subpath \"{}\")", escape_profile_string(a)))
                .collect::<Vec<_>>()
                .join(" ");
            let mut profile = format!(
                "(version 1)\n(allow default)\n(deny file-write* (require-not (require-any {subpaths})))\n"
            );
            profile.push_str(XPC_DISK_SERVICE_DENIES);
            profile
        }
        SandboxMode::DangerFullAccess => unreachable!("danger-full-access never confines"),
    }
}

/// True when `sandbox-exec` is present and functional; cached per process.
fn sandbox_exec_available() -> bool {
    // The probe profile must allow exec: `(allow default)` is required —
    // without it even `execvp("true")` is denied and the probe always fails
    // on hosts where sandbox-exec works fine.
    const PROBE_PROFILE: &str = "(version 1) (allow default)";
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg(PROBE_PROFILE)
            .arg("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|st| st.success())
            .unwrap_or(false)
    })
}

/// Runner selection for the seatbelt backend. The host-capability probe is
/// cached per process; the `STARRIVER_SANDBOX_RUNNER` override is re-read on
/// every call so env changes take effect without a restart (the sticky
/// selection cache made a forced-unavailable state depend on which caller
/// initialized it first). Same override semantics as the linux/windows probe
/// chains: `STARRIVER_SANDBOX_RUNNER` accepts only `seatbelt` (the other
/// platforms' runners do not exist here), a forced runner must still probe
/// healthy, and any other value is rejected — an explicit override never
/// silently falls back.
fn seatbelt_selected() -> Result<(), String> {
    match std::env::var("STARRIVER_SANDBOX_RUNNER").ok().as_deref() {
        Some("seatbelt") | None => {
            if sandbox_exec_available() {
                Ok(())
            } else {
                Err("sandbox-exec not found on this host".into())
            }
        }
        Some(other) => Err(format!(
            "STARRIVER_SANDBOX_RUNNER=`{other}` is not one of seatbelt"
        )),
    }
}

/// macOS Seatbelt sandbox provider.
pub struct SeatbeltSandboxProvider;

impl SeatbeltSandboxProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SeatbeltSandboxProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxProvider for SeatbeltSandboxProvider {
    fn confine(
        &self,
        argv: Vec<String>,
        policy: &SandboxPolicy,
    ) -> Result<ConfinedArgv, SandboxError> {
        match policy.mode {
            SandboxMode::DangerFullAccess => Ok(ConfinedArgv::new(argv, SandboxEnforcement::Full)),
            mode => {
                if let Err(detail) = seatbelt_selected() {
                    return Err(SandboxError::Unavailable { mode, detail });
                }
                let profile = seatbelt_profile(mode, &policy.workspace_root);
                let mut wrapped = vec!["sandbox-exec".into(), "-p".into(), profile, "--".into()];
                wrapped.extend(argv);
                Ok(ConfinedArgv::new(wrapped, SandboxEnforcement::Full).runner("seatbelt"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_profile_denies_file_writes() {
        let profile = seatbelt_profile(SandboxMode::ReadOnly, "/work");
        assert!(profile.contains("(allow default)"));
        assert!(profile.contains("(deny file-write*)"));
        assert!(!profile.contains("(require-any"));
        // `file-write-data*` is an undefined seatbelt variable: the whole profile
        // gets rejected, and every bash command in a read-only session fails
        // (not just writes).
        assert!(
            !profile.contains("file-write-data*"),
            "invalid seatbelt variable must not appear:\n{profile}"
        );
    }

    #[test]
    fn read_only_profile_parses_and_stays_read_only() {
        // Regression (2026-09-02): the ReadOnly profile once retained `(deny
        // file-write-data*)`, so sandbox-exec failed to parse it outright
        // (exit 65, "unbound variable: file-write-data* at line 4") and read-only
        // sessions could not even run echo/ls. After the fix: commands execute
        // and writes are still denied.
        if !std::path::Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let profile = seatbelt_profile(SandboxMode::ReadOnly, "/work");
        let run = |args: &[&str]| {
            std::process::Command::new("sandbox-exec")
                .arg("-p")
                .arg(&profile)
                .arg("sh")
                .arg("-c")
                .args(args)
                .status()
                .map(|st| st.success())
                .unwrap_or(false)
        };
        assert!(
            run(&["true"]),
            "read-only profile must parse and exec:\n{profile}"
        );
        let probe = std::env::temp_dir().join(format!("sb-readonly-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&probe);
        let write_cmd = format!("touch {}", probe.display());
        assert!(
            !run(&[&write_cmd]),
            "read-only profile must deny writes (probe: {})",
            probe.display()
        );
        assert!(!probe.exists(), "read-only must not create files");
    }

    #[test]
    fn workspace_write_profile_grants_workspace_and_tmp() {
        let profile = seatbelt_profile(SandboxMode::WorkspaceWrite, "/work");
        assert!(profile.contains("(require-any (subpath \"/work\") (subpath \""));
        assert!(profile.contains("(deny file-write* (require-not"));
    }

    #[test]
    fn workspace_write_profile_is_valid_seatbelt_syntax() {
        // Regression: the profile also emitted `(deny file-write-data* …)`,
        // an undefined seatbelt operation variable — sandbox-exec rejected
        // the whole profile ("unbound variable: file-write-data*") once the
        // availability probe started passing.
        let profile = seatbelt_profile(SandboxMode::WorkspaceWrite, "/work");
        assert!(
            !profile.contains("file-write-data*"),
            "invalid seatbelt variable must not appear:\n{profile}"
        );
        if std::path::Path::new("/usr/bin/sandbox-exec").exists() {
            let ok = std::process::Command::new("sandbox-exec")
                .arg("-p")
                .arg(&profile)
                .arg("sh")
                .arg("-c")
                .arg("true")
                .status()
                .map(|st| st.success())
                .unwrap_or(false);
            assert!(
                ok,
                "generated workspace-write profile must parse and exec:\n{profile}"
            );
        }
    }

    #[test]
    fn danger_mode_returns_argv_unchanged() {
        let provider = SeatbeltSandboxProvider::new();
        let policy = SandboxPolicy {
            mode: SandboxMode::DangerFullAccess,
            workspace_root: "/work".into(),
            session_id: None,
        };
        let confined = provider
            .confine(vec!["sh".into(), "-c".into(), "true".into()], &policy)
            .unwrap();
        assert_eq!(
            confined.argv,
            vec!["sh".to_string(), "-c".to_string(), "true".to_string()]
        );
    }

    #[test]
    fn unavailable_error_carries_stable_code() {
        let error = SandboxError::Unavailable {
            mode: SandboxMode::ReadOnly,
            detail: "sandbox-exec not found on this host".into(),
        };
        assert_eq!(error.code(), "SANDBOX_UNAVAILABLE");
    }

    #[test]
    fn probe_succeeds_when_sandbox_exec_is_installed() {
        // Regression: the probe profile lacked `(allow default)`, so execvp
        // was denied and sandbox-exec looked "not found" on healthy hosts.
        if std::path::Path::new("/usr/bin/sandbox-exec").exists() {
            assert!(
                sandbox_exec_available(),
                "probe must succeed when sandbox-exec is installed"
            );
        }
    }
}

/// Escape a path for a seatbelt profile string literal: an unescaped `"` or
/// `\` in a workspace path either breaks profile parsing (every bash call
/// fails) or injects extra directives widening the writable set.
fn escape_profile_string(path: &str) -> String {
    path.replace('\u{5c}', "\u{5c}\u{5c}").replace('"', "\u{5c}\"")
}
