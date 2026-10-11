//! Permission catalog: permissions declared by methods and permissions held
//! by callers (clients/plugin frames).

use serde::{Deserialize, Serialize};

// ── Permission constants (stable strings, reused by clients and plugin
// manifests) ──

pub const PERM_SESSION_EXECUTE: &str = "session:execute";
pub const PERM_SESSION_READ: &str = "session:read";
pub const PERM_SESSION_WRITE: &str = "session:write";
pub const PERM_APPROVAL_RESPOND: &str = "approval:respond";
pub const PERM_TOOLS_READ: &str = "tools:read";
pub const PERM_MODELS_READ: &str = "models:read";
pub const PERM_MODELS_WRITE: &str = "models:write";
pub const PERM_CREDENTIALS_WRITE: &str = "credentials:write";
pub const PERM_PLUGINS_READ: &str = "plugins:read";
pub const PERM_PLUGINS_WRITE: &str = "plugins:write";
pub const PERM_SKILLS_READ: &str = "skills:read";
pub const PERM_SKILLS_WRITE: &str = "skills:write";
pub const PERM_WORKSPACES_READ: &str = "workspaces:read";
pub const PERM_WORKSPACES_WRITE: &str = "workspaces:write";
pub const PERM_AGENTS_READ: &str = "agents:read";
pub const PERM_AGENTS_WRITE: &str = "agents:write";
pub const PERM_CREDENTIALS_READ: &str = "credentials:read";
pub const PERM_MEMORY_READ: &str = "memory:read";
pub const PERM_MEMORY_WRITE: &str = "memory:write";
pub const PERM_MCP_READ: &str = "mcp:read";
pub const PERM_MCP_WRITE: &str = "mcp:write";
pub const PERM_SCHEDULED_READ: &str = "scheduled:read";
pub const PERM_SCHEDULED_WRITE: &str = "scheduled:write";
pub const PERM_JOBS_READ: &str = "jobs:read";
pub const PERM_SOURCES_READ: &str = "sources:read";
pub const PERM_SOURCES_WRITE: &str = "sources:write";
pub const PERM_MARKET_READ: &str = "market:read";
pub const PERM_MARKET_WRITE: &str = "market:write";
pub const PERM_PROJECT_WRITE: &str = "project:write";
pub const PERM_HOOKS_READ: &str = "hooks:read";
/// WI-115: manage hook scripts (create/update/delete under ~/.starriver/hooks).
pub const PERM_HOOKS_WRITE: &str = "hooks:write";
pub const PERM_REVIEW_START: &str = "review:start";
pub const PERM_ATTACHMENTS_READ: &str = "attachments:read";
pub const PERM_ATTACHMENTS_WRITE: &str = "attachments:write";
pub const PERM_UI_CONTRIB: &str = "ui:contrib";
/// Full shell replacement (elevated permission, not granted by default).
pub const PERM_SHELL_REPLACE: &str = "shell:replace";

/// Caller identity and permissions: the main client holds everything;
/// plugin frames hold a subset per their manifest allowlist.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallerScope {
    /// Stable identifier: `"builtin"` or `"plugin:<name>"`.
    pub scope: String,
    #[serde(default)]
    pub permissions: Vec<String>,
}

impl CallerScope {
    pub fn builtin() -> Self {
        Self {
            scope: "builtin".into(),
            permissions: Vec::new(),
        }
    }

    pub fn plugin(name: &str, permissions: Vec<String>) -> Self {
        Self {
            scope: format!("plugin:{name}"),
            permissions,
        }
    }

    /// The main client (builtin) is treated as having all permissions;
    /// plugin frames are checked against their allowlist.
    pub fn allows(&self, required: &[&str]) -> bool {
        if self.scope == "builtin" {
            return true;
        }
        required
            .iter()
            .all(|permission| self.permissions.iter().any(|p| p == permission))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_scope_allows_everything() {
        let caller = CallerScope::builtin();
        assert!(caller.allows(&[PERM_SESSION_EXECUTE, PERM_CREDENTIALS_WRITE]));
    }

    #[test]
    fn plugin_scope_enforces_whitelist() {
        let caller = CallerScope::plugin(
            "demo",
            vec![PERM_SESSION_EXECUTE.into(), PERM_SESSION_READ.into()],
        );
        assert!(caller.allows(&[PERM_SESSION_EXECUTE]));
        assert!(!caller.allows(&[PERM_CREDENTIALS_WRITE]));
    }
}
