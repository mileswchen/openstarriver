//! Subagent delegation configuration (`subagent authorize selectable
//! child models` parity): built-in templates can be enabled and bound to an
//! exact model key.

use crate::settings::SubagentSettings;
use crate::{types, HostRuntime};

impl HostRuntime {
    /// Merge the built-in subagent templates with stored delegation config.
    pub fn list_subagent_configs(&self) -> Vec<types::SubagentConfig> {
        let settings = self.settings.read().unwrap();
        crate::subagent::builtin_templates()
            .into_iter()
            .map(|t| {
                let cfg = settings.subagents.entries.iter().find(|e| e.name == t.name);
                types::SubagentConfig {
                    name: t.name.to_string(),
                    description: t.description.to_string(),
                    tools: t.tools.iter().map(|s| s.to_string()).collect(),
                    read_only: t.read_only,
                    enabled: cfg.map(|c| c.enabled).unwrap_or(false),
                    model: cfg.and_then(|c| c.model.clone()),
                }
            })
            .collect()
    }

    /// Enable/disable one template and bind it to a model key.
    pub fn set_subagent_config(
        &self,
        name: String,
        enabled: bool,
        model: Option<String>,
    ) -> anyhow::Result<()> {
        let known = crate::subagent::builtin_templates()
            .iter()
            .any(|t| t.name == name);
        if !known {
            return Err(anyhow::anyhow!("unknown subagent template `{name}`"));
        }
        let mut settings = self.settings.write().unwrap();
        settings.subagents.entries.retain(|e| e.name != name);
        if enabled || model.is_some() {
            settings.subagents.entries.push(SubagentSettings {
                name,
                enabled,
                model,
                acp: None,
            });
        }
        settings
            .save()
            .map_err(|e| anyhow::anyhow!("settings: {e}"))?;
        drop(settings);
 // schema-layer parity: the tool description names the enabled
        // templates, so an enable/disable must re-register the definition.
        self.reinstall_subagent_tool();
        Ok(())
    }
}
