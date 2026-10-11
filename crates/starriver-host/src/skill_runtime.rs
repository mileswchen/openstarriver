use dashmap::DashMap;
use starriver_skills::profile::skill_manager::SkillManager;
use std::sync::Arc;

/// Runtime skill registry — loads and executes AgentSkills
pub struct SkillRuntime {
    manager: Arc<SkillManager>,
    active: Arc<DashMap<String, bool>>,
}

impl SkillRuntime {
    pub fn new() -> Self {
        let manager = Arc::new(SkillManager::new());
        // Scan common skill directories
        let search_dirs: Vec<std::path::PathBuf> = {
            let mut dirs = Vec::new();
            if let Ok(cwd) = std::env::current_dir() {
                dirs.push(cwd.join(".starriver").join("skills"));
            }
            if let Some(home) = std::env::var("HOME")
                .ok()
                .or_else(|| std::env::var("USERPROFILE").ok())
            {
                dirs.push(
                    std::path::PathBuf::from(home)
                        .join(".starriver")
                        .join("skills"),
                );
            }
            dirs
        };
        for dir in &search_dirs {
            if dir.exists() {
                if let Err(e) = manager.load_dir(dir) {
                    tracing::warn!(dir = %dir.display(), error = %e, "Skill dir scan failed");
                }
            }
        }
        Self {
            manager,
            active: Arc::new(DashMap::new()),
        }
    }

    pub fn scan_custom_dir(&self, dir: &std::path::Path) -> anyhow::Result<Vec<String>> {
        self.manager.load_dir(dir)
    }

    pub fn list(&self) -> Vec<String> {
        self.manager
            .list_all()
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    pub fn activate(&self, name: &str) -> bool {
        if self.manager.get(name).is_some() {
            self.active.insert(name.to_string(), true);
            true
        } else {
            false
        }
    }

    pub fn deactivate(&self, name: &str) {
        self.active.remove(name);
    }

    /// List all skills as SkillInfo structs (with activation-mode metadata + body).
    pub fn list_skills(&self) -> Vec<crate::types::SkillInfo> {
        self.manager
            .list_all()
            .into_iter()
            .map(|s| {
                let has_keyword = s
                    .conditions
                    .as_ref()
                    .and_then(|c| c.trigger.as_ref())
                    .map(|t| !t.patterns.is_empty())
                    .unwrap_or(false);
                let has_suffix = s
                    .conditions
                    .as_ref()
                    .map(|c| !c.match_file_suffix.is_empty())
                    .unwrap_or(false);
                let has_hooks = s
                    .conditions
                    .as_ref()
                    .and_then(|c| c.hooks.as_ref())
                    .is_some()
                    || s.hooks.is_some();
                crate::types::SkillInfo {
                    name: s.name.clone(),
                    description: s.description.clone().unwrap_or_default(),
                    has_command: s.command.is_some(),
                    command: s.command.clone(),
                    version: s.version.clone(),
                    always_on: s.always_on,
                    has_keyword,
                    has_suffix,
                    has_hooks,
                    enabled: self.active.contains_key(&s.name),
                    body: s.body.clone(),
                }
            })
            .collect()
    }

    /// Toggle a skill's active state (enabled/disabled) for the agent prompt.
    pub fn set_enabled(&self, name: &str, enabled: bool) -> bool {
        if self.manager.get(name).is_some() {
            if enabled {
                self.active.insert(name.to_string(), true);
            } else {
                self.active.remove(name);
            }
            true
        } else {
            false
        }
    }

    /// Names of currently active skills (for prompt injection).
    pub fn active_names(&self) -> Vec<String> {
        self.active.iter().map(|e| e.key().clone()).collect()
    }

    /// Rescan default skill directories
    pub fn scan_default_dirs(&self) {
        let search_dirs: Vec<std::path::PathBuf> = {
            let mut dirs = Vec::new();
            if let Ok(cwd) = std::env::current_dir() {
                dirs.push(cwd.join(".starriver").join("skills"));
            }
            if let Some(home) = std::env::var("HOME")
                .ok()
                .or_else(|| std::env::var("USERPROFILE").ok())
            {
                dirs.push(
                    std::path::PathBuf::from(home)
                        .join(".starriver")
                        .join("skills"),
                );
            }
            dirs
        };
        for dir in &search_dirs {
            if dir.exists() {
                if let Err(e) = self.manager.load_dir(dir) {
                    tracing::warn!(dir = %dir.display(), error = %e, "Skill dir scan failed");
                }
            }
        }
    }

    pub fn is_active(&self, name: &str) -> bool {
        self.active.contains_key(name)
    }

    /// Get the system prompt injection from all active skills
    pub fn active_skill_prompt(&self, base_prompt: &str) -> String {
        let names: Vec<String> = self.active.iter().map(|e| e.key().clone()).collect();
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        self.manager
            .inject_to_system_prompt(base_prompt, &name_refs)
    }

    pub fn manager(&self) -> Arc<SkillManager> {
        self.manager.clone()
    }
}

impl Default for SkillRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_runtime_creates() {
        let rt = SkillRuntime::new();
        let _ = rt.list();
    }

    #[test]
    fn test_skill_runtime_activate_nonexistent() {
        let rt = SkillRuntime::new();
        assert!(!rt.activate("nonexistent-skill"));
    }

    #[test]
    fn test_skill_runtime_is_active() {
        let rt = SkillRuntime::new();
        assert!(!rt.is_active("any"));
    }

    #[test]
    fn test_active_skill_prompt_empty() {
        let rt = SkillRuntime::new();
        let prompt = rt.active_skill_prompt("Base prompt.");
        assert!(prompt.contains("Base prompt."));
    }
}
