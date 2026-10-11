//! Permission preset: pins a fresh session's sandbox mode + approval policy
//! (permission-presets alignment; default workspace-write + manual).
//! The preset is refreshed from the execution-mode profile before every
//! execute (`PluginKernel::preset`), so brand-new sessions inherit the mode's
//! defaults while resumed sessions keep their own per-session policy.

use starriver_agent_loop::approval::{effective_approval_mode, effective_policy, ApprovalMode};
use starriver_kernel::json::JsonValue;
use starriver_kernel::sandbox::{effective_sandbox_mode, SandboxMode};
use starriver_kernel::session::{Session, SessionCreated};
use std::sync::Arc;

/// One preset bundle: sandbox mode + approval policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub sandbox: SandboxMode,
    pub approval: ApprovalMode,
}

impl Default for Preset {
    fn default() -> Self {
        Self {
            sandbox: SandboxMode::WorkspaceWrite,
            approval: ApprovalMode::Manual,
        }
    }
}

/// ApprovalMode constant used for execution-mode presets (used by plugin_kernel to assemble the Preset).
/// The Auto presets for Ptc/Goal now read uniformly from execution_mode::profile.
pub const MANUAL_APPROVAL: ApprovalMode = ApprovalMode::Manual;

/// Pin the preset into a fresh session; sessions that already carry either
/// knob (resume/seeded) are left untouched.
pub fn pin_initial_preset(session: &Session, preset: Preset) {
    if effective_sandbox_mode(session).is_none() {
        let _ = session.append(
            "sandbox/mode",
            JsonValue::Object(vec![(
                "mode".into(),
                JsonValue::String(preset.sandbox.as_str().into()),
            )]),
        );
    }
    // Both folds must be checked: effective_policy only recognizes the legacy
    // values ask/never and returns None for manual/accept-edits/auto/bypass —
    // checking only it would miss sessions already stamped, causing re-stamping
    // on every resume and overriding the user's per-session choice.
    if effective_policy(session).is_none() && effective_approval_mode(session).is_none() {
        let _ = session.append(
            "approval/policy",
            JsonValue::Object(vec![(
                "policy".into(),
                JsonValue::String(preset.approval.as_str().into()),
            )]),
        );
    }
}

/// Register the `session/created` listener on the agent-loop shared bus. The
/// preset is read live so tests and the UI can swap it before creating a
/// session.
pub fn register_preset_pinner_with(
    bus: &cordis_rslite::EventBus,
    preset: Arc<std::sync::Mutex<Preset>>,
) -> cordis_rslite::EffectHandle {
    bus.on_sync::<SessionCreated, _>(move |created| {
        pin_initial_preset(&created.session, *preset.lock().unwrap());
        Ok(cordis_rslite::Value::new(()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_session_gets_default_preset() {
        let session = Session::new(
            starriver_kernel::session::SessionId("s1".into()),
            cordis_rslite::EventBus::new(),
        );
        pin_initial_preset(&session, Preset::default());
        assert_eq!(
            effective_sandbox_mode(&session),
            Some(SandboxMode::WorkspaceWrite)
        );
        // Default approval is now the 4-value Manual ("manual"); the legacy
        // Ask/Never fold no longer recognizes it and falls back to Ask.
        assert_eq!(effective_policy(&session), None);
        assert_eq!(
            starriver_agent_loop::approval::effective_approval_mode(&session),
            Some(ApprovalMode::Manual)
        );
    }

    #[test]
    fn pinned_session_is_not_repinned() {
        let session = Session::new(
            starriver_kernel::session::SessionId("s1".into()),
            cordis_rslite::EventBus::new(),
        );
        pin_initial_preset(&session, Preset::default());
        let count_before = session.events().len();
        pin_initial_preset(&session, Preset::default());
        assert_eq!(session.events().len(), count_before);
    }

    #[test]
    fn session_created_event_pins_preset() {
        let bus = cordis_rslite::EventBus::new();
        let preset = Arc::new(std::sync::Mutex::new(Preset::default()));
        register_preset_pinner_with(&bus, preset);
        let session = Session::new(
            starriver_kernel::session::SessionId("s1".into()),
            bus.clone(),
        );
        bus.emit(Arc::new(SessionCreated {
            session: session.clone(),
        }));
        assert_eq!(
            effective_sandbox_mode(&session),
            Some(SandboxMode::WorkspaceWrite)
        );
        assert_eq!(
            starriver_agent_loop::approval::effective_approval_mode(&session),
            Some(ApprovalMode::Manual)
        );
    }
}
