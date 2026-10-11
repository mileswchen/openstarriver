//! Permission presets (`permission-presets` parity): one preset bundles a
//! sandbox mode with an approval mode, so the composer surfaces a single
//! picker instead of two independent knobs. Selection appends both
//! `sandbox/mode` and `approval/policy` events; anything the log folds to
//! that matches no preset renders as the derived `custom` state.

use starriver_agent_loop::approval::ApprovalMode;
use starriver_kernel::sandbox::SandboxMode;

/// One named (sandbox, approval) bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionPreset {
    pub name: &'static str,
    pub sandbox: SandboxMode,
    pub approval: ApprovalMode,
    /// UI hint: the entry carries the high-risk confirmation gate.
    pub danger: bool,
}

/// Built-in preset table (default-table parity, adapted to starriver's
/// four-value approval vocabulary): the three workspace-write tiers keep the
/// existing approval ladder; the danger preset bundles full access with
/// never-ask — behaviorally the approval half is moot there (the pre-execute
/// waterfall short-circuits under DangerFullAccess), the bundling makes the
/// semantics explicit.
pub fn builtin_presets() -> Vec<PermissionPreset> {
    use ApprovalMode::{AcceptEdits, Auto, ByPass, Manual};
    use SandboxMode::{DangerFullAccess, WorkspaceWrite};
    vec![
        PermissionPreset {
            name: "manual",
            sandbox: WorkspaceWrite,
            approval: Manual,
            danger: false,
        },
        PermissionPreset {
            name: "accept-edits",
            sandbox: WorkspaceWrite,
            approval: AcceptEdits,
            danger: false,
        },
        PermissionPreset {
            name: "auto",
            sandbox: WorkspaceWrite,
            approval: Auto,
            danger: false,
        },
        PermissionPreset {
            name: "danger-full-access",
            sandbox: DangerFullAccess,
            approval: ByPass,
            danger: true,
        },
    ]
}

/// Look up one preset by name (exact match).
pub fn parse(name: &str) -> Option<PermissionPreset> {
    builtin_presets().into_iter().find(|p| p.name == name)
}

/// Derived display state: fold the session's (sandbox, approval) knobs and
/// resolve them against the table. `None` = the `custom` not-a-preset state
/// (reserves the name; it is never a selectable target). Legacy logs
/// whose `approval/policy` carries `ask`/`never` fold to `None` approval and
/// land here too.
pub fn derive_preset(
    sandbox: Option<SandboxMode>,
    approval: Option<ApprovalMode>,
) -> Option<&'static str> {
    let (sandbox, approval) = match (sandbox, approval) {
        (Some(s), Some(a)) => (s, a),
        _ => return None,
    };
    builtin_presets()
        .into_iter()
        .find(|p| p.sandbox == sandbox && p.approval == approval)
        .map(|p| p.name)
}

/// The picker's seed value for a fresh session: the same bundle the execution
/// mode presets stamp (Standard = workspace-write + manual).
pub fn default_preset() -> PermissionPreset {
    parse("manual").unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_four_entries_with_unique_names() {
        let presets = builtin_presets();
        assert_eq!(presets.len(), 4);
        let mut names: Vec<&str> = presets.iter().map(|p| p.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn danger_preset_bundles_full_access_with_bypass() {
        let d = parse("danger-full-access").unwrap();
        assert!(d.danger);
        assert_eq!(d.sandbox, SandboxMode::DangerFullAccess);
        assert_eq!(d.approval, ApprovalMode::ByPass);
    }

    #[test]
    fn workspace_tiers_keep_the_existing_approval_ladder() {
        for (name, approval) in [
            ("manual", ApprovalMode::Manual),
            ("accept-edits", ApprovalMode::AcceptEdits),
            ("auto", ApprovalMode::Auto),
        ] {
            let p = parse(name).unwrap();
            assert_eq!(p.sandbox, SandboxMode::WorkspaceWrite);
            assert_eq!(p.approval, approval);
            assert!(!p.danger);
        }
    }

    #[test]
    fn parse_rejects_unknown_and_custom() {
        assert!(parse("nope").is_none());
        assert!(parse("custom").is_none());
        assert!(parse("read-only").is_none());
    }

    #[test]
    fn derive_matches_exact_bundles_only() {
        assert_eq!(
            derive_preset(
                Some(SandboxMode::WorkspaceWrite),
                Some(ApprovalMode::AcceptEdits)
            ),
            Some("accept-edits")
        );
        assert_eq!(
            derive_preset(
                Some(SandboxMode::DangerFullAccess),
                Some(ApprovalMode::ByPass)
            ),
            Some("danger-full-access")
        );
        // Non-table combos and missing knobs are custom.
        assert_eq!(
            derive_preset(Some(SandboxMode::ReadOnly), Some(ApprovalMode::Manual)),
            None
        );
        assert_eq!(
            derive_preset(
                Some(SandboxMode::WorkspaceWrite),
                Some(ApprovalMode::ByPass)
            ),
            None
        );
        assert_eq!(derive_preset(None, Some(ApprovalMode::Manual)), None);
    }

    #[test]
    fn default_preset_is_manual() {
        assert_eq!(default_preset().name, "manual");
    }
}
