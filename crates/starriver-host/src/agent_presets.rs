//! Agent-preset roster (WI-5, ui-agent-preset parity): the model-visible
//! face of an agent — sampling, tool whitelist, prompt assembly — as one
//! selectable composition, discovered from AGENT.md frontmatter.
//!
//! Plan boundaries: the preset never carries the model route (`model:` keeps
//! flowing through the agent-bindings plane) or any permission state
//! (execution-mode plane) — `AgentPresetSpec` rejects those keys at parse
//! time, and the host surfaces the failure as a `broken` roster row (visible,
//! not selectable) instead of a silent fallback.

use starriver_kernel::agent::AgentOptions;
use starriver_skills::profile::agent::AgentPresetSpec;

use crate::HostRuntime;

/// One selectable preset on the roster. `id` is the slugified agent name;
/// `agent` is the hub name the run path resolves (identical for `builtin`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPresetRow {
    pub id: String,
    pub agent: String,
    pub name: String,
    pub description: String,
 /// `builtin` | `user` — the discovery root (trust layering; the
    /// workspace/user split lands with the workspace-scoped agents roots).
    pub source: &'static str,
    /// Parse/validation failure: the row renders but cannot be selected.
    pub broken: Option<String>,
}

pub const BUILTIN_CODING_PRESET_ID: &str = "coding";

/// Slugify a hub agent name into a roster id (the memory layer's slug rules —
/// lowercase, non-alphanumerics collapsed).
fn slugify_name(name: &str) -> String {
    starriver_memory::slugify(name)
}

/// Parse one agent directory's AGENT.md down to its preset spec.
/// `Ok(None)` = the file has no `preset:` block (composition falls back to
/// the built-in coding preset); `Err(reason)` = broken.
pub fn load_agent_preset_spec(
    agent_dir: &std::path::Path,
) -> Result<Option<AgentPresetSpec>, String> {
    let path = agent_dir.join("AGENT.md");
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let doc = starriver_skills::profile::parser::parse_document(&content)
        .map_err(|e| format!("invalid AGENT.md: {e}"))?;
    let profile =
        starriver_skills::profile::agent::AgentProfile::try_from(doc).map_err(|e| e.to_string())?;
    match profile.preset {
        None => Ok(None),
        Some(Ok(spec)) => Ok(Some(spec)),
        Some(Err(reason)) => Err(reason),
    }
}

/// Field-wise fold of an AGENT.md `preset:` block over the base coding
/// preset: declared keys override, absent keys keep the built-in composition.
pub fn apply_preset_spec(mut base: AgentOptions, spec: Option<&AgentPresetSpec>) -> AgentOptions {
    let Some(spec) = spec else {
        return base;
    };
    if spec.max_tokens.is_some() {
        base.max_tokens = spec.max_tokens;
    }
    if spec.context_window.is_some() {
        base.context_window = spec.context_window;
    }
    if spec.time_refresh_interval_ms.is_some() {
        base.time_refresh_interval_ms = spec.time_refresh_interval_ms;
    }
    if let Some(visible_tools) = &spec.visible_tools {
        base.visible_tools = Some(visible_tools.clone());
    }
    if let Some(prompt) = &spec.prompt {
        let mut prompt_settings = base.prompt_settings.take().unwrap_or_default();
        if let Some(include_identity) = prompt.include_identity {
            prompt_settings.include_identity = include_identity;
        }
        if let Some(include_runtime_context) = prompt.include_runtime_context {
            prompt_settings.include_runtime_context = include_runtime_context;
        }
        if let Some(instructions_max_bytes) = prompt.instructions_max_bytes {
            prompt_settings.instructions_max_bytes = Some(instructions_max_bytes as usize);
        }
        base.prompt_settings = Some(prompt_settings);
    }
    base
}

impl HostRuntime {
    /// Discover the preset roster: the built-in `coding` composition plus
    /// every hub agent's AGENT.md. Re-parsed per call so a freshly authored
 /// preset is selectable without a restart (re-reads its roots too).
    /// The hub carries a backfilled AGENT.md twin of each built-in agent; it
    /// is the user-editable copy, not a second agent, so roster rows dedupe
    /// by agent name. Order: Starriver Code, Chat Agent, rest alphabetically.
    pub fn list_agent_presets(&self) -> Vec<AgentPresetRow> {
        let mut rows = vec![AgentPresetRow {
            id: BUILTIN_CODING_PRESET_ID.into(),
            agent: crate::CODING_AGENT_NAME.into(),
            name: "Starriver Code".into(),
            description: "Built-in full coding composition (all tools, default prompt).".into(),
            source: "builtin",
            broken: None,
        }];
        let mut hub_rows: Vec<AgentPresetRow> = Vec::new();
        for info in self.hub.list() {
            let id = slugify_name(&info.name);
            let taken = rows
                .iter()
                .chain(hub_rows.iter())
                .any(|row| row.id == id || row.agent.eq_ignore_ascii_case(&info.name));
            if taken {
                continue; // first root wins a duplicate id/agent
            }
            let broken = load_agent_preset_spec(&info.path)
                .err()
                .map(|reason| format!("preset unavailable: {reason}"));
            hub_rows.push(AgentPresetRow {
                id,
                agent: info.name.clone(),
                name: info.name.clone(),
                description: info.description.clone(),
                source: "user",
                broken,
            });
        }
        hub_rows.sort_by(|a, b| {
            let a_chat = a.agent == crate::CHAT_AGENT_NAME;
            let b_chat = b.agent == crate::CHAT_AGENT_NAME;
            b_chat.cmp(&a_chat).then_with(|| a.name.cmp(&b.name))
        });
        rows.extend(hub_rows);
        rows
    }

    /// Compose and register the `AgentOptions` for every healthy preset id
    /// (the loop registry's stored form): the built-in coding preset with the
    /// profile's declared overrides folded field-wise. Runs before each
    /// execute and roster read so both the create and resume paths resolve
    /// the same composition — including after a process restart.
    pub fn sync_agent_presets(&self) {
        let base = {
            let settings = self.settings.read().unwrap_or_else(|e| e.into_inner());
            crate::plugin_kernel::coding_preset(&settings)
        };
        let kernel = {
            let inner = self.inner.blocking_lock();
            let Some(runtime) = inner.as_ref() else {
                return;
            };
            runtime.agent.clone()
        };
        for row in self.list_agent_presets() {
            if row.broken.is_some() {
                continue; // never registered: selection is rejected at the door
            }
            let spec = if row.source == "builtin" {
                None
            } else {
                self.hub
                    .list()
                    .iter()
                    .find(|info| slugify_name(&info.name) == row.id)
                    .and_then(|info| load_agent_preset_spec(&info.path).ok().flatten())
            };
            kernel.upsert_agent_preset(
                row.id.clone(),
                apply_preset_spec(base.clone(), spec.as_ref()),
            );
        }
    }
}
