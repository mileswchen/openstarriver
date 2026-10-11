//! Sandbox seam: file-effect modes, per-call policy, and the provider
//! interface implemented by the concrete backends in `starriver-sandbox`
//! (Seatbelt on macOS, bubblewrap/Landlock on Linux).

use crate::session::Session;

/// File-effect sandbox mode (vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl SandboxMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::DangerFullAccess => "danger-full-access",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read-only" => Some(SandboxMode::ReadOnly),
            "workspace-write" => Some(SandboxMode::WorkspaceWrite),
            "danger-full-access" => Some(SandboxMode::DangerFullAccess),
            _ => None,
        }
    }
}

impl std::fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One confined execution's file-effect policy (per call).
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    pub workspace_root: String,
    pub session_id: Option<String>,
}

/// Enforcement completeness of the selected backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxEnforcement {
    Full,
    Partial,
}

/// Hook applied to the spawn `Command` inside the forked child just before
/// exec (via `pre_exec`), for backends that confine the process itself
/// (Landlock) rather than by wrapping argv (Seatbelt, bubblewrap). An error
/// fails the spawn — fail closed, never run unconfined.
pub type SpawnHook =
    std::sync::Arc<dyn Fn(&mut tokio::process::Command) -> std::io::Result<()> + Send + Sync>;

/// Result of wrapping argv for confined execution.
#[derive(Clone)]
pub struct ConfinedArgv {
    pub argv: Vec<String>,
    pub enforcement: SandboxEnforcement,
    /// Selected backend identity: "none" | "seatbelt" | "bwrap" | "landlock".
    pub runner: String,
    pub spawn_hook: Option<SpawnHook>,
}

impl std::fmt::Debug for ConfinedArgv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfinedArgv")
            .field("argv", &self.argv)
            .field("enforcement", &self.enforcement)
            .field("runner", &self.runner)
            .field("spawn_hook", &self.spawn_hook.is_some())
            .finish()
    }
}

impl ConfinedArgv {
    pub fn new(argv: Vec<String>, enforcement: SandboxEnforcement) -> Self {
        Self {
            argv,
            enforcement,
            runner: "none".into(),
            spawn_hook: None,
        }
    }

    pub fn runner(mut self, runner: &str) -> Self {
        self.runner = runner.into();
        self
    }

    pub fn spawn_hook(mut self, hook: SpawnHook) -> Self {
        self.spawn_hook = Some(hook);
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("sandbox mode \"{mode}\" requested but no backend is usable: {detail}")]
    Unavailable { mode: SandboxMode, detail: String },
    #[error("sandbox confine failed: {0}")]
    Confine(String),
}

impl SandboxError {
 /// Stable wire code, aligned with `SANDBOX_UNAVAILABLE`.
    pub fn code(&self) -> &'static str {
        match self {
            SandboxError::Unavailable { .. } => "SANDBOX_UNAVAILABLE",
            SandboxError::Confine(_) => "SANDBOX_CONFINE_FAILED",
        }
    }
}

/// Wraps a command so it runs confined under a per-call policy. Does NOT
/// extend `Service` (the `Service::NAME` const breaks object safety);
/// concrete providers implement `Service` themselves and are registered by
/// their concrete type.
pub trait SandboxProvider: Send + Sync + 'static {
    fn confine(
        &self,
        argv: Vec<String>,
        policy: &SandboxPolicy,
    ) -> Result<ConfinedArgv, SandboxError>;
}

/// Pure fold: the last `sandbox/mode` event in log order.
pub fn effective_sandbox_mode(session: &Session) -> Option<SandboxMode> {
    for event in session.events().into_iter().rev() {
        if event.event_type == "sandbox/mode" {
            return event.data.as_str_at("mode").and_then(SandboxMode::parse);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::JsonValue;
    use crate::session::SessionId;

    #[test]
    fn mode_tags_round_trip() {
        for mode in [
            SandboxMode::ReadOnly,
            SandboxMode::WorkspaceWrite,
            SandboxMode::DangerFullAccess,
        ] {
            assert_eq!(SandboxMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(SandboxMode::parse("rogue"), None);
    }

    #[test]
    fn effective_mode_fold_is_last_wins() {
        let session = Session::new(SessionId("s1".into()), cordis_rslite::EventBus::new());
        session
            .append(
                "sandbox/mode",
                JsonValue::Object(vec![("mode".into(), JsonValue::String("read-only".into()))]),
            )
            .unwrap();
        session
            .append(
                "sandbox/mode",
                JsonValue::Object(vec![(
                    "mode".into(),
                    JsonValue::String("workspace-write".into()),
                )]),
            )
            .unwrap();
        assert_eq!(
            effective_sandbox_mode(&session),
            Some(SandboxMode::WorkspaceWrite)
        );
    }

    #[test]
    fn effective_mode_without_event_is_none() {
        let session = Session::new(SessionId("s1".into()), cordis_rslite::EventBus::new());
        assert_eq!(effective_sandbox_mode(&session), None);
    }

    #[test]
    fn sandbox_error_carries_stable_code() {
        let error = SandboxError::Unavailable {
            mode: SandboxMode::ReadOnly,
            detail: "no backend".into(),
        };
        assert_eq!(error.code(), "SANDBOX_UNAVAILABLE");
    }
}
