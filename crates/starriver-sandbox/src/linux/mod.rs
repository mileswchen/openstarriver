//! Linux sandbox: probe-chain selection between the bubblewrap runner
//! (bubblewrap binary, mount namespaces — Full enforcement) and the native
//! Landlock runner (kernel LSM, in-process — Partial enforcement).

pub mod bwrap;
pub mod landlock;

use std::sync::OnceLock;

use starriver_kernel::sandbox::{
    ConfinedArgv, SandboxEnforcement, SandboxError, SandboxMode, SandboxPolicy, SandboxProvider,
};

use crate::probe::{self, Probe};

pub struct LinuxSandbox;

impl LinuxSandbox {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LinuxSandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl SandboxProvider for LinuxSandbox {
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
                match runner {
                    "bwrap" => bwrap::confine(argv, mode, &policy.workspace_root),
                    "landlock" => landlock::confine(argv, mode, &policy.workspace_root),
                    other => Err(SandboxError::Confine(format!("unknown runner `{other}`"))),
                }
            }
        }
    }
}

/// Probe chain `bwrap → landlock`. The probes are cached per process (host
/// capability, env-independent); the `STARRIVER_SANDBOX_RUNNER` override is
/// re-read on every call so env changes take effect without a restart. It
/// forces a specific runner for support/diagnostics: the named runner must
/// probe healthy, otherwise the chain reports unavailable (explicit beats
/// silent fallback).
fn select_runner() -> Result<&'static str, String> {
    static PROBES: OnceLock<Vec<Probe>> = OnceLock::new();
    let probes = PROBES.get_or_init(|| vec![bwrap_probe(), landlock_probe()]);
    match std::env::var("STARRIVER_SANDBOX_RUNNER").ok().as_deref() {
        Some(forced @ ("bwrap" | "landlock")) => {
            let probe = probes
                .iter()
                .find(|p| p.runner == forced)
                .expect("both runners always probed");
            if probe.available {
                Ok(probe.runner)
            } else {
                Err(format!(
                    "forced runner {forced} unavailable: {}",
                    probe.detail
                ))
            }
        }
        Some(other) => Err(format!(
            "STARRIVER_SANDBOX_RUNNER=`{other}` is not one of bwrap/landlock"
        )),
        None => probe::pick(probes),
    }
}

fn bwrap_probe() -> Probe {
    let (available, detail) = bwrap::available();
    Probe {
        runner: "bwrap",
        available,
        detail,
    }
}

fn landlock_probe() -> Probe {
    let (available, detail) = landlock::available();
    Probe {
        runner: "landlock",
        available,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn danger_mode_returns_argv_unchanged() {
        let policy = SandboxPolicy {
            mode: SandboxMode::DangerFullAccess,
            workspace_root: "/work".into(),
            session_id: None,
        };
        let confined = LinuxSandbox::new()
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
    fn probe_chain_selects_a_runner_on_capable_hosts() {
        // This host is the WI-11 reference environment: bubblewrap 0.12 +
        // kernel 6.12 (Landlock ABI 6). At least one runner must probe
        // healthy here; which one wins depends on unprivileged userns policy.
        let picked = select_runner().expect("at least one runner available on this host");
        assert!(picked == "bwrap" || picked == "landlock");
    }
}
