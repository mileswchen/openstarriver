pub mod admin;
pub mod admin2;
pub mod admin3;
pub mod admin4;
pub mod admin_credentials;
pub mod admin_jobs;
pub mod admin_subagents;
pub mod agent_config;
pub mod agent_presets;
pub mod agents;
pub mod approval_bridge;
pub mod attachments;
pub mod e2b;
pub mod execution_mode;
pub mod export;
pub mod helpers;
pub mod hooks;
pub mod host_store;
pub mod logging;
pub mod lsp;
pub mod market;
pub mod mcp;
pub mod mcp_status_state;
pub mod memory_pipeline;
pub mod model_registry;
pub mod permission_presets;
pub mod plan_mode;
pub mod plan_review_bridge;
pub mod plugin;
pub mod plugin_kernel;
pub mod preset;
pub mod prompt_override;
pub mod prompts;
pub mod protocol_json;
pub mod providers;
pub mod question_bridge;
pub mod runtime;
pub mod sandbox;
pub mod scheduler;
pub mod script_executor;
pub mod sessions;
pub mod settings;
pub mod skill_runtime;
pub mod slash;
pub mod subagent;
pub mod terminal;
pub mod title;
pub mod tools;
pub mod types;
pub mod ui_channel;
pub mod workspace;

#[cfg(test)]
mod legacy_tests;

#[cfg(test)]
mod testutil;

pub use runtime::HostRuntime;

pub use script_executor::ScriptExecutor;

use std::sync::Arc;

/// Test observation hook: read the `llm.providers` slice straight off disk.
#[doc(hidden)]
pub fn host_settings_llm_providers(
    home: &std::path::Path,
) -> Vec<crate::settings::LlmProviderSettings> {
    crate::settings::Settings::from_file(&home.join(".starriver").join("settings.yaml"))
        .llm
        .providers
}

/// Fixed name of the built-in Coding Agent (shared identity across host / tui / desktop).
pub const CODING_AGENT_NAME: &str = "Starriver Code";

/// Fixed name of the built-in Chat Agent — the agent `session.execute` runs
/// as when the caller names none. Not configurable: there is no global
/// "current agent" any more; every run names its agent explicitly or gets
/// this one.
pub const CHAT_AGENT_NAME: &str = "Chat Agent";

/// Split a "provider/variant" key into (provider, variant) using registered provider ids. Pure function.
pub fn split_model_key<'a>(key: &'a str, provider_ids: &[&str]) -> Option<(String, &'a str)> {
    provider_ids
        .iter()
        .filter(|pid| key.starts_with(&format!("{pid}/")))
        .max_by_key(|pid| pid.len())
        .map(|pid| (pid.to_string(), &key[pid.len() + 1..]))
}

/// Resolve an agent's `model.default` key into (variant, provider). Pure function.
pub fn resolve_agent_model_key(
    default_key: Option<String>,
    provider_ids: &[String],
) -> (Option<String>, Option<String>) {
    let Some(key) = default_key else {
        return (None, None);
    };
    let provider_refs: Vec<&str> = provider_ids.iter().map(|s| s.as_str()).collect();
    match split_model_key(&key, &provider_refs) {
        Some((pid, vid)) => (Some(vid.to_string()), Some(pid)),
        None => (None, None),
    }
}

/// Validate the project working directory (formerly desktop `resolve_project_dir`). Pure function.
pub fn resolve_project_dir(cwd: Option<String>) -> Result<String, String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let path = cwd
        .map(|c| c.replace("~/", &format!("{}/", home)))
        .ok_or_else(|| {
            "working directory is required — create it first and pick it here".to_string()
        })?;
    if path.trim().is_empty() {
        return Err("project cwd cannot be empty".into());
    }
    if !std::path::Path::new(&path).is_dir() {
        return Err(format!(
            "working directory does not exist: {path} — create it first"
        ));
    }
    Ok(path)
}

/// Internal runtime handle: tokio runtime + loop engine + session store.
pub struct RuntimeInner {
    pub runtime: tokio::runtime::Runtime,
    /// Kernel event bus shared by coordinator, HostStore projection, agents.
    pub bus: cordis_rslite::EventBus,
    pub agent: Arc<crate::plugin_kernel::PluginKernel>,
    /// Kernel session persistence (coordinator) for admin reads/writes.
    pub persistence: Arc<starriver_session_persistence::SqliteSessionPersistence>,
    /// Host-owned session metadata: workspace registry + fail-soft projection.
    pub host_store: Arc<crate::host_store::HostStore>,
    /// WI-97: goal state per session (objective-driven autonomous loop).
    pub goals: std::sync::Mutex<std::collections::HashMap<String, GoalState>>,
}

/// WI-97: per-session goal state.
#[derive(Debug, Clone)]
pub struct GoalState {
    pub objective: String,
    pub token_budget: Option<u64>,
    pub tokens_used: u64,
    pub status: GoalStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalStatus {
    Active,
    Paused,
    BudgetLimited,
    Complete,
}

impl GoalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::BudgetLimited => "budgetLimited",
            GoalStatus::Complete => "complete",
        }
    }
}
