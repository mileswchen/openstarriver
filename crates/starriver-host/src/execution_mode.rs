//! Profiles and parsing for executionMode (Standard/Plan/PTC/Goal).
//!
//! Each execution mode carries a preset sandboxMode/approvalMode;
//! explicit user override > mode preset > global fallback (Standard preset).
//! Plan mode has a phase lock: the planning phase is forced ReadOnly;
//! after confirmation it enters the execution phase.

use starriver_agent_loop::approval::ApprovalMode;
use starriver_kernel::sandbox::SandboxMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Standard,
    Plan,
    Ptc,
    Goal,
}

impl ExecutionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutionMode::Standard => "standard",
            ExecutionMode::Plan => "plan",
            ExecutionMode::Ptc => "ptc",
            ExecutionMode::Goal => "goal",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "standard" => Some(ExecutionMode::Standard),
            "plan" => Some(ExecutionMode::Plan),
            "ptc" => Some(ExecutionMode::Ptc),
            "goal" => Some(ExecutionMode::Goal),
            _ => None,
        }
    }
}

/// The preset sandboxMode/approvalMode (and allowed sets) for an execution mode.
#[derive(Debug, Clone)]
pub struct ExecutionModeProfile {
    pub preset_sandbox: SandboxMode,
    pub preset_approval: ApprovalMode,
    pub allowed_sandboxes: &'static [SandboxMode],
    pub allowed_approvals: &'static [ApprovalMode],
}

const READ_ONLY: SandboxMode = SandboxMode::ReadOnly;
const WORKSPACE_WRITE: SandboxMode = SandboxMode::WorkspaceWrite;
const DANGER_FULL: SandboxMode = SandboxMode::DangerFullAccess;

const MANUAL: ApprovalMode = ApprovalMode::Manual;
const ACCEPT_EDITS: ApprovalMode = ApprovalMode::AcceptEdits;
const AUTO: ApprovalMode = ApprovalMode::Auto;
const BY_PASS: ApprovalMode = ApprovalMode::ByPass;

const ALL_SANDBOXES: &[SandboxMode] = &[READ_ONLY, WORKSPACE_WRITE, DANGER_FULL];
const ALL_APPROVALS: &[ApprovalMode] = &[MANUAL, ACCEPT_EDITS, AUTO, BY_PASS];

pub fn profile(mode: ExecutionMode) -> ExecutionModeProfile {
    match mode {
        ExecutionMode::Standard => ExecutionModeProfile {
            preset_sandbox: WORKSPACE_WRITE,
            // A-1 adopted (2026-09-02, strengthened): factory default and Standard preset are both Manual.
            // AcceptEdits is no longer the automatic preset of any mode; it can only be chosen explicitly via the session-level dropdown.
            preset_approval: MANUAL,
            allowed_sandboxes: ALL_SANDBOXES,
            allowed_approvals: ALL_APPROVALS,
        },
        ExecutionMode::Plan => ExecutionModeProfile {
            // Plan's execution-phase preset; the planning phase forces ReadOnly (handled by set_execution_mode).
            preset_sandbox: WORKSPACE_WRITE,
            preset_approval: MANUAL,
            allowed_sandboxes: &[WORKSPACE_WRITE, DANGER_FULL],
            // Plan strict lock: approval is always Manual; no user override allowed (locked in both planning and execution phases).
            allowed_approvals: &[MANUAL],
        },
        ExecutionMode::Ptc => ExecutionModeProfile {
            preset_sandbox: WORKSPACE_WRITE,
            preset_approval: AUTO,
            allowed_sandboxes: ALL_SANDBOXES,
            allowed_approvals: ALL_APPROVALS,
        },
        ExecutionMode::Goal => ExecutionModeProfile {
            preset_sandbox: WORKSPACE_WRITE,
            preset_approval: AUTO,
            allowed_sandboxes: ALL_SANDBOXES,
            allowed_approvals: ALL_APPROVALS,
        },
    }
}

/// Priority resolution: explicit user choice > executionMode preset.
/// Plan's planning phase is forced ReadOnly by the caller (not via this function);
/// Plan's approval is unaffected by this function — it is hard-locked to Manual
/// by [`approval_locked`], which the user cannot override.
pub fn resolve_effective(
    mode: ExecutionMode,
    user_sandbox: Option<SandboxMode>,
    user_approval: Option<ApprovalMode>,
) -> (SandboxMode, ApprovalMode) {
    let p = profile(mode);
    (
        user_sandbox.unwrap_or(p.preset_sandbox),
        user_approval.unwrap_or(p.preset_approval),
    )
}

/// Plan strict lock: under this execution mode the approval mode cannot be overridden by the user (always Manual).
/// Enforced jointly by `session.setApprovalMode` and the preset stamping on every execute turn.
pub fn approval_locked(mode: ExecutionMode) -> bool {
    matches!(mode, ExecutionMode::Plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_mode_parses_round_trip() {
        for mode in [
            ExecutionMode::Standard,
            ExecutionMode::Plan,
            ExecutionMode::Ptc,
            ExecutionMode::Goal,
        ] {
            assert_eq!(ExecutionMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(ExecutionMode::parse("unknown"), None);
    }

    #[test]
    fn standard_preset_is_workspace_write_manual() {
        // A-1 strengthened: the Standard preset approval is also Manual (conservative out of the box);
        // AcceptEdits can only be chosen explicitly by the user at the session level.
        let p = profile(ExecutionMode::Standard);
        assert_eq!(p.preset_sandbox, SandboxMode::WorkspaceWrite);
        assert_eq!(p.preset_approval, ApprovalMode::Manual);
        // AcceptEdits remains in Standard's allowed set (users may select it explicitly; it just isn't the automatic preset).
        assert!(p.allowed_approvals.contains(&ApprovalMode::AcceptEdits));
    }

    #[test]
    fn plan_preset_is_workspace_write_manual() {
        let p = profile(ExecutionMode::Plan);
        assert_eq!(p.preset_sandbox, SandboxMode::WorkspaceWrite);
        assert_eq!(p.preset_approval, ApprovalMode::Manual);
        // Plan does not allow ReadOnly (the planning phase is already locked; the execution phase needs write access).
        assert!(!p.allowed_sandboxes.contains(&READ_ONLY));
        // Plan strict lock: only Manual approval is allowed.
        assert_eq!(p.allowed_approvals, &[MANUAL]);
        assert!(approval_locked(ExecutionMode::Plan));
        assert!(!approval_locked(ExecutionMode::Standard));
    }

    #[test]
    fn resolve_effective_user_overrides_preset() {
        let p = profile(ExecutionMode::Standard);
        // User did not override → preset takes effect
        let (s, a) = resolve_effective(ExecutionMode::Standard, None, None);
        assert_eq!(s, p.preset_sandbox);
        assert_eq!(a, p.preset_approval);
        // User override (e.g. accept-edits → manual) → the user's value wins
        let (s, a) = resolve_effective(
            ExecutionMode::Standard,
            Some(SandboxMode::ReadOnly),
            Some(ApprovalMode::Manual),
        );
        assert_eq!(s, SandboxMode::ReadOnly);
        assert_eq!(a, ApprovalMode::Manual);
    }
}
