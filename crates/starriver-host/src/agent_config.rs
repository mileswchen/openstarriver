//! Per-agent execution configuration, aggregated from its durable homes.
//!
//! `AgentState` used to cache these fields in one mutable global snapshot;
//! each field now has an authoritative source and the run path asks for the
//! aggregate at execute time instead:
//!
//! | field                                  | source                                              |
//! |----------------------------------------|-----------------------------------------------------|
//! | tools / model / memory / isolate default | AGENT.md profile (hub, re-read from disk)         |
//! | project_id / working_dir               | per-agent activation (`settings.agent.active_workspaces`) validated against the host workspace registry |
//! | reasoning_effort                       | named override table (`HostRuntime::reasoning_overrides`) |
//!
//! [`assemble`] is the pure mapping (table-tested); [`HostRuntime::resolve_agent_config`]
//! gathers the inputs from the runtime. The persona body itself is not part of
//! the config: the kernel persona resident carries it.

use starriver_memory::MemoryConfig;
use starriver_skills::profile::agent::AgentProfile;

use crate::{resolve_agent_model_key, HostRuntime};

/// Everything `execute` needs to know about an agent for one run.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentConfig {
    pub agent_name: String,
    /// Profile `tools:` rendered to runtime tool names (`script:`/`mcp_` prefixed).
    pub profile_tools: Vec<String>,
    /// Model variant / provider id split off the profile's `model.default`;
    /// both `None` when the key does not match a configured provider.
    pub model: Option<String>,
    pub model_id: Option<String>,
    /// Effective reasoning override ("low"|"high"|"max"); `None` follows the
    /// provider's configured effort (an explicit "off" also lands here).
    pub reasoning_effort: Option<String>,
    pub memory: MemoryConfig,
    pub project_id: Option<String>,
    pub working_dir: Option<String>,
    /// Per-agent default for new-session worktree isolation (profile `isolate:`).
    pub isolate_default: bool,
}

/// Pure mapping from an agent's durable inputs to its execution config.
/// `profile == None` (unknown agent) yields the empty config the snapshot-less
/// path used to run with: no tools, no model, default memory, no cwd.
pub fn assemble(
    agent_name: &str,
    profile: Option<&AgentProfile>,
    provider_ids: &[String],
    workspace: Option<(String, String)>,
    reasoning_override: Option<Option<String>>,
) -> AgentConfig {
    let default_key = profile
        .and_then(|p| p.model.as_ref())
        .map(|m| m.default.clone());
    let (model, model_id) = resolve_agent_model_key(default_key, provider_ids);
    let (project_id, working_dir) = match workspace {
        Some((id, path)) => (Some(id), Some(path)),
        None => (None, None),
    };
    AgentConfig {
        agent_name: agent_name.to_string(),
        profile_tools: profile
            .map(HostRuntime::profile_tool_names)
            .unwrap_or_default(),
        model,
        model_id,
        reasoning_effort: reasoning_override.flatten(),
        memory: profile.and_then(|p| p.memory.clone()).unwrap_or_default(),
        project_id,
        working_dir,
        isolate_default: profile.and_then(|p| p.isolate).unwrap_or(false),
    }
}

impl HostRuntime {
    /// Aggregate `agent_name`'s execution config from its durable sources.
    pub fn resolve_agent_config(&self, agent_name: &str) -> AgentConfig {
        let profile = self.hub.get(agent_name);
        let provider_ids: Vec<String> = self
            .settings
            .read()
            .unwrap()
            .llm
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect();
        let workspace = self.resolve_workspace_binding(agent_name);
        let reasoning = self.agent_reasoning_override(agent_name);
        assemble(
            agent_name,
            profile.as_ref(),
            &provider_ids,
            workspace,
            reasoning,
        )
    }

    /// Verify `agent_name` exists and make it the kernel's active persona:
    /// the profile body goes into the persona resident and the profile's
    /// `skills:` are activated (the `skills:always-on` resident re-reads the
    /// activation set on every assembly). No-op when the same agent already
    /// occupies the residents — `execute` calls this per run, so back-to-back
    /// runs of one agent skip the swap entirely.
    ///
    /// Review mode is only disturbed by an actual agent change: a first run
    /// (no resident yet) and same-agent reruns leave an active review
    /// resident untouched — review is an explicit user action and outlives
    /// turns until the user switches agents (the replacement semantic the
    /// old `switch_agent` carried) or starts a new review.
    pub(crate) fn ensure_agent_residency(&self, agent_name: &str) -> anyhow::Result<()> {
        let profile = self
            .hub
            .get(agent_name)
            .ok_or_else(|| anyhow::anyhow!("Agent not found: {agent_name}"))?;
        let occupied = self
            .persona_agent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if occupied.as_deref() == Some(agent_name) {
            return Ok(());
        }
        for skill_name in &profile.skills {
            self.skill_runtime.activate(skill_name);
        }
        // E7 wiring: the persona slot carries the raw profile body (skills
        // have their own resident). An actual switch also leaves review mode;
        // a first registration does not (nothing to switch away from).
        let switching = occupied.is_some();
        let inner_arc = self.inner.clone();
        let body = profile.body.clone();
        let name = agent_name.to_string();
        self.runtime_handle.block_on(async move {
            let l = inner_arc.lock().await;
            let Some(runtime) = l.as_ref() else { return };
            if switching {
                runtime.agent.exit_review_mode().await;
            }
            if let Err(error) = runtime.agent.set_persona(Some(body)).await {
                tracing::warn!(agent = %name, error = %error, "kernel persona registration failed");
            }
        });
        *self.persona_agent.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(agent_name.to_string());
        Ok(())
    }

    /// Currently resident persona agent (observation hook).
    #[doc(hidden)]
    pub fn debug_resident_agent(&self) -> Option<String> {
        self.persona_agent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Observation hook: the working dir `agent_name`'s runs would use.
    #[doc(hidden)]
    pub fn resolve_workspace_dir(&self, agent_name: &str) -> Option<String> {
        self.resolve_agent_config(agent_name).working_dir
    }

    /// The agent's activated workspace `(id, path)`: the persisted activation
    /// validated against the host workspace registry — a stale id (workspace
    /// deleted while the app was away) binds nothing.
    pub(crate) fn resolve_workspace_binding(&self, agent_name: &str) -> Option<(String, String)> {
        let workspace_id = self
            .settings
            .read()
            .unwrap()
            .agent
            .active_workspaces
            .get(agent_name)
            .cloned()?;
        self.workspace_path_of(&workspace_id)
            .map(|path| (workspace_id, path))
    }

    /// Registry lookup: the directory of workspace `workspace_id`, if registered.
    pub(crate) fn workspace_path_of(&self, workspace_id: &str) -> Option<String> {
        let inner = self.inner.clone();
        self.runtime_handle.block_on(async {
            let l = inner.lock().await;
            l.as_ref().and_then(|runtime| {
                runtime
                    .host_store
                    .list_workspaces()
                    .into_iter()
                    .find(|workspace| workspace.id == workspace_id)
                    .map(|workspace| workspace.path)
            })
        })
    }

    /// Persist `agent_name`'s workspace activation (`None` clears it) in
    /// settings.yaml — best-effort: a failed write only costs the
    /// restore-on-restart convenience.
    pub(crate) fn persist_workspace_activation(
        &self,
        agent_name: &str,
        workspace_id: Option<&str>,
    ) {
        {
            let mut settings = self.settings.write().unwrap();
            match workspace_id {
                Some(wid) => {
                    settings
                        .agent
                        .active_workspaces
                        .insert(agent_name.to_string(), wid.to_string());
                }
                None => {
                    settings.agent.active_workspaces.remove(agent_name);
                }
            }
        }
        if let Ok(yaml) = serde_yaml::to_string(&*self.settings.read().unwrap()) {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            let path = std::path::PathBuf::from(home)
                .join(".starriver")
                .join("settings.yaml");
            let _ = std::fs::write(&path, yaml);
        }
    }

    /// Coding-agent convenience: when the coding agent has no persisted
    /// workspace and the registry holds exactly one, adopt it. Activations
    /// made before the persistence existed never landed in settings.yaml; a
    /// single-project user then keeps their workspace, branch chip and sandbox
    /// cwd across restarts without re-clicking. Other agents are deliberately
    /// excluded: Chat must stay workspace-less so its sessions keep out of
    /// Coding's lists. Callers invoke this at the natural adoption points
    /// (UI workspace flows / execute), not on a timer.
    pub(crate) fn adopt_single_workspace_for_coding_agent(&self) {
        let name = crate::CODING_AGENT_NAME;
        let has_persisted = self
            .settings
            .read()
            .unwrap()
            .agent
            .active_workspaces
            .contains_key(name);
        if has_persisted {
            return;
        }
        let inner = self.inner.clone();
        let all = self.runtime_handle.block_on(async {
            let l = inner.lock().await;
            l.as_ref()
                .map(|runtime| runtime.host_store.list_workspaces())
                .unwrap_or_default()
        });
        if all.len() == 1 {
            self.persist_workspace_activation(name, Some(&all[0].id));
        }
    }

    /// Test hook for the private adoption path.
    #[doc(hidden)]
    pub fn adopt_single_workspace_for_coding_agent_test(&self) {
        self.adopt_single_workspace_for_coding_agent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_skills::profile::types::{ModelConfig, ToolRef};

    fn profile(model: Option<&str>, tools: Vec<ToolRef>, isolate: Option<bool>) -> AgentProfile {
        AgentProfile {
            name: "Agent".into(),
            version: None,
            description: None,
            author: None,
            icon: None,
            skills: vec![],
            tools,
            mode: None,
            model: model.map(|default| ModelConfig {
                default: default.into(),
                models: vec![],
                routing: vec![],
                fallback: None,
            }),
            permissions: None,
            hooks: None,
            commands: vec![],
            ui: None,
            memory: None,
            isolate,
            preset: None,
            body: "persona".into(),
        }
    }

    fn providers() -> Vec<String> {
        vec!["deepseek".into(), "openai".into()]
    }

    #[test]
    fn unknown_agent_yields_empty_config() {
        let config = assemble("Ghost", None, &providers(), None, None);
        assert_eq!(
            config,
            AgentConfig {
                agent_name: "Ghost".into(),
                profile_tools: vec![],
                model: None,
                model_id: None,
                reasoning_effort: None,
                memory: MemoryConfig::default(),
                project_id: None,
                working_dir: None,
                isolate_default: false,
            }
        );
    }

    #[test]
    fn model_key_splits_into_variant_and_provider() {
        let p = profile(Some("deepseek/deepseek-chat"), vec![], None);
        let config = assemble("Agent", Some(&p), &providers(), None, None);
        assert_eq!(config.model.as_deref(), Some("deepseek-chat"));
        assert_eq!(config.model_id.as_deref(), Some("deepseek"));
    }

    #[test]
    fn model_key_with_unknown_provider_resolves_to_none() {
        let p = profile(Some("anthropic/claude"), vec![], None);
        let config = assemble("Agent", Some(&p), &providers(), None, None);
        assert_eq!((config.model, config.model_id), (None, None));
    }

    #[test]
    fn workspace_binding_fills_project_and_working_dir() {
        let p = profile(None, vec![], None);
        let config = assemble(
            "Agent",
            Some(&p),
            &providers(),
            Some(("ws-1".into(), "/tmp/proj".into())),
            None,
        );
        assert_eq!(config.project_id.as_deref(), Some("ws-1"));
        assert_eq!(config.working_dir.as_deref(), Some("/tmp/proj"));
    }

    #[test]
    fn reasoning_override_flattens_off_to_none() {
        let p = profile(None, vec![], None);
        let follow = assemble("Agent", Some(&p), &providers(), None, None);
        let off = assemble("Agent", Some(&p), &providers(), None, Some(None));
        let high = assemble(
            "Agent",
            Some(&p),
            &providers(),
            None,
            Some(Some("high".into())),
        );
        assert_eq!(follow.reasoning_effort, None);
        assert_eq!(off.reasoning_effort, None);
        assert_eq!(high.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn profile_tools_render_runtime_names() {
        let p = profile(
            None,
            vec![
                ToolRef::Builtin("read".into()),
                ToolRef::Script("deploy".into()),
                ToolRef::Mcp("github".into()),
            ],
            None,
        );
        let config = assemble("Agent", Some(&p), &providers(), None, None);
        assert_eq!(
            config.profile_tools,
            vec!["read", "script:deploy", "mcp_github"]
        );
    }

    #[test]
    fn isolate_default_follows_profile_flag() {
        let on = profile(None, vec![], Some(true));
        let off = profile(None, vec![], Some(false));
        let unset = profile(None, vec![], None);
        assert!(assemble("A", Some(&on), &providers(), None, None).isolate_default);
        assert!(!assemble("A", Some(&off), &providers(), None, None).isolate_default);
        assert!(!assemble("A", Some(&unset), &providers(), None, None).isolate_default);
    }

    #[test]
    fn memory_section_overrides_default() {
        let mut p = profile(None, vec![], None);
        p.memory = Some(MemoryConfig {
            top_k: 42,
            ..MemoryConfig::default()
        });
        let config = assemble("Agent", Some(&p), &providers(), None, None);
        assert_eq!(config.memory.top_k, 42);
    }
}
