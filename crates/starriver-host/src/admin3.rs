use std::path::PathBuf;

use crate::helpers::builtin_provider_presets;
use crate::{types, HostRuntime};

impl HostRuntime {
    pub fn list_model_providers(&self) -> Vec<types::ModelProvider> {
        let reg = self.provider_registry.read().unwrap();
        builtin_provider_presets(|id| reg.is_enabled(id))
    }

    pub fn export_agent(&self, name: String) -> anyhow::Result<String> {
        let profile = self
            .hub
            .get(&name)
            .ok_or_else(|| anyhow::anyhow!("Agent not found: {name}"))?;
        // Quote both scalars: names/versions come from user frontmatter and
        // may contain YAML indicator characters (`: `, `#`, quotes) — an
        // unquoted export cannot be re-imported by parse_document.
        Ok(format!(
            "---\nname: {:?}\nversion: {:?}\n---\n\n{}\n",
            profile.name,
            profile.version.unwrap_or_else(|| "0.1.0".into()),
            profile.body
        ))
    }

    pub fn create_agent(
        &self,
        name: String,
        description: String,
    ) -> anyhow::Result<types::AgentInfo> {
        // The name becomes a directory path component — restrict to a safe
        // charset instead of allowing traversal out of the agents dir.
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        anyhow::ensure!(valid, "agent name must be [A-Za-z0-9_-]");
        // Quoted YAML scalars: a description containing ": " or "#" used to
        // emit frontmatter serde_yaml cannot parse back (the agent then
        // silently vanishes on the next scan).
        let quoted_description = format!("{:?}", description.replace('\n', " "));
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        let agent_dir = home.join(".starriver").join("agents").join(&name);
        std::fs::create_dir_all(&agent_dir).map_err(|e| anyhow::anyhow!("mkdir: {e}"))?;
        std::fs::write(
            agent_dir.join("AGENT.md"),
            format!(
                "---\nname: {name}\nversion: 0.1.0\ndescription: {quoted_description}\n---\n\n# {name}\n"
            ),
        )
        .map_err(|e| anyhow::anyhow!("write: {e}"))?;
        self.hub.scan().map_err(|e| anyhow::anyhow!("scan: {e}"))?;
        Ok(types::AgentInfo {
            name,
            description,
            version: "0.1.0".into(),
        })
    }
}
