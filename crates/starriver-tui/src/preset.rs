//! Sandbox preset picker model: preset list / validation / copy text (pure
//! functions, fully unit tested; rendering stays in `ui.rs`).
//!
//! A "preset" = one executionMode (standard / plan / ptc / goal). Each carries a
//! preset sandboxMode + approvalMode and the allowed per-session approval
//! overrides. The values are derived from [`starriver_host::execution_mode`]'s
//! profile table (not hand-copied) so the TUI list cannot drift from host
//! behavior; a unit test re-derives the table and asserts equality.
//!
//! Switching calls two host APIs (see `host.rs`):
//! - `set_execution_mode(mode)` — global preset switch (settings + kernel);
//! - `set_approval_mode(session_id, mode)` — per-session approval override.

use starriver_host::execution_mode::{profile, ExecutionMode};

use crate::i18n::{self, Lang};

/// Preset ids in picker order (mirrors `ExecutionMode`).
pub const PRESET_IDS: &[&str] = &["standard", "plan", "ptc", "goal"];

/// One selectable preset row (values derived from the host profile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetInfo {
    pub id: &'static str,
    /// i18n key of the display name.
    pub name_key: &'static str,
    /// i18n key of the description.
    pub desc_key: &'static str,
    /// Preset sandboxMode (`SandboxMode::as_str` spelling).
    pub sandbox: &'static str,
    /// Preset approvalMode (`ApprovalMode::as_str` spelling).
    pub approval: &'static str,
    /// Allowed per-session approval overrides (empty for locked presets).
    pub approvals: Vec<&'static str>,
}

/// Derive the preset table from the host profile (single source of truth).
pub fn presets() -> Vec<PresetInfo> {
    PRESET_IDS
        .iter()
        .map(|&id| {
            let em = ExecutionMode::parse(id).expect("PRESET_IDS match ExecutionMode");
            let prof = profile(em);
            // Locked presets (Plan: approval hard-locked to manual) expose no
            // override level — the picker applies them in one step.
            let approvals: Vec<&'static str> = if host_is_plan_locked(em) {
                Vec::new()
            } else {
                prof.allowed_approvals.iter().map(|a| a.as_str()).collect()
            };
            PresetInfo {
                id,
                name_key: preset_name_key(id),
                desc_key: preset_desc_key(id),
                sandbox: prof.preset_sandbox.as_str(),
                approval: prof.preset_approval.as_str(),
                approvals,
            }
        })
        .collect()
}

/// `plan`'s approval is hard-locked to manual (`approval_locked`); the picker
/// hides the approval level for it. Exposed for the drift-guard test.
pub fn host_is_plan_locked(em: ExecutionMode) -> bool {
    starriver_host::execution_mode::approval_locked(em)
}

fn preset_name_key(id: &str) -> &'static str {
    match id {
        "plan" => "preset.plan.name",
        "ptc" => "preset.ptc.name",
        "goal" => "preset.goal.name",
        _ => "preset.standard.name",
    }
}

fn preset_desc_key(id: &str) -> &'static str {
    match id {
        "plan" => "preset.plan.desc",
        "ptc" => "preset.ptc.desc",
        "goal" => "preset.goal.desc",
        _ => "preset.standard.desc",
    }
}

/// Find a preset by id (case-insensitive); the picker/`/preset <name>` validation.
pub fn find(id: &str) -> Option<PresetInfo> {
    let lower = id.trim().to_lowercase();
    presets().into_iter().find(|p| p.id == lower)
}

/// Localized preset display name.
pub fn preset_name(p: &PresetInfo, lang: Lang) -> String {
    i18n::tr(lang, p.name_key).to_string()
}

/// Picker row: current marker + name + preset sandbox/approval values
/// (language only affects the name; the profile values are language-neutral).
pub fn row_label(p: &PresetInfo, lang: Lang, is_current: bool) -> String {
    let marker = if is_current { "●" } else { " " };
    format!(
        "{marker} {}  ·  sandbox {} · approval {}",
        preset_name(p, lang),
        p.sandbox,
        p.approval
    )
}

/// Approval choices for the second picker level: `None` (follow the preset
/// default) first, then the allowed overrides. Locked presets yield `None`
/// only via [`Self::approval_choices`] being skipped by the caller.
pub fn approval_options(p: &PresetInfo) -> Vec<Option<&'static str>> {
    let mut out = vec![None];
    out.extend(p.approvals.iter().map(|a| Some(*a)));
    out
}

/// Localized approval choice label; `None` renders "follow preset (<preset>)".
pub fn approval_label(choice: Option<&str>, p: &PresetInfo, lang: Lang) -> String {
    match choice {
        None => i18n::fmt!(i18n::tr(lang, "preset.approval.default"), p.approval),
        Some(a) => i18n::tr(lang, &format!("preset.approval.{a}")).to_string(),
    }
}

/// Whether the preset applies in one step (no approval level).
pub fn is_single_step(p: &PresetInfo) -> bool {
    p.approvals.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zh() -> Lang {
        Lang::Zh
    }

    /// Drift guard: the derived table must equal the host profile values.
    #[test]
    fn presets_match_host_profiles() {
        for p in presets() {
            let em = ExecutionMode::parse(p.id).unwrap();
            let prof = profile(em);
            assert_eq!(p.sandbox, prof.preset_sandbox.as_str());
            assert_eq!(p.approval, prof.preset_approval.as_str());
            if host_is_plan_locked(em) {
                assert!(p.approvals.is_empty(), "{} locked", p.id);
            } else {
                let allowed: Vec<&str> =
                    prof.allowed_approvals.iter().map(|a| a.as_str()).collect();
                assert_eq!(p.approvals, allowed);
            }
        }
    }

    #[test]
    fn table_shape_is_stable() {
        let all = presets();
        let ids: Vec<&str> = all.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["standard", "plan", "ptc", "goal"]);
        // Host profile values (A-1: Standard preset approval is Manual).
        let std = find("standard").unwrap();
        assert_eq!(std.sandbox, "workspace-write");
        assert_eq!(std.approval, "manual");
        let plan = find("plan").unwrap();
        assert!(is_single_step(&plan)); // approval hard-locked
        assert_eq!(find("ptc").unwrap().approval, "auto");
        assert_eq!(find("goal").unwrap().approval, "auto");
    }

    #[test]
    fn find_is_case_insensitive_and_trims() {
        assert_eq!(find("Plan").unwrap().id, "plan");
        assert_eq!(find("  GOAL ").unwrap().id, "goal");
        assert!(find("bogus").is_none());
        assert!(find("").is_none());
    }

    #[test]
    fn row_label_shows_values_and_current_marker() {
        let p = find("standard").unwrap();
        let cur = row_label(&p, zh(), true);
        let other = row_label(&p, zh(), false);
        assert!(cur.starts_with("●"));
        assert!(other.starts_with(' '));
        assert!(cur.contains("sandbox workspace-write"));
        assert!(cur.contains("approval manual"));
        // Language changes the display name only.
        assert!(row_label(&p, Lang::En, false).contains("Standard"));
    }

    #[test]
    fn approval_options_default_first_then_allowed() {
        let p = find("standard").unwrap();
        let opts = approval_options(&p);
        assert_eq!(opts[0], None);
        let values: Vec<&str> = opts.iter().flatten().copied().collect();
        assert_eq!(values, vec!["manual", "accept-edits", "auto", "bypass"]);
    }

    #[test]
    fn approval_label_localizes_default_and_choices() {
        let p = find("ptc").unwrap();
        assert_eq!(
            approval_label(None, &p, zh()),
            "跟随预设（auto）".to_string()
        );
        assert_eq!(approval_label(Some("manual"), &p, zh()), "手动确认");
        assert_eq!(
            approval_label(Some("accept-edits"), &p, Lang::En),
            "Accept edits"
        );
        assert_eq!(
            approval_label(Some("bypass"), &p, zh()),
            "完全放行（需授权）"
        );
    }
}
