use std::path::PathBuf;

use starriver_api_macro::api_method;

use crate::{types, HostRuntime};
use starriver_kernel::agent::AgentRegistry;

pub(crate) fn build_skill_md(
    name: &str,
    description: &str,
    command: Option<&str>,
    body: &str,
) -> Result<String, String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "Invalid skill name '{name}': use letters, digits, '-' or '_'"
        ));
    }
    if body.trim().is_empty() {
        return Err("Skill body (instructions) cannot be empty".into());
    }
    let mut frontmatter = format!("---\nname: {name}\n");
    if !description.trim().is_empty() {
        frontmatter += &format!("description: {:?}\n", description.trim());
    }
    frontmatter += "version: \"0.1.0\"\n";
    if let Some(cmd) = command.map(str::trim).filter(|c| !c.is_empty()) {
        // Quoted like description — a command containing ": " or "#" used
        // to emit frontmatter serde_yaml rejects (skill lost on rescan).
        frontmatter += &format!("command: {:?}\n", cmd);
    }
    frontmatter += "---\n\n";
    Ok(format!("{frontmatter}{body}"))
}

pub(crate) fn set_always_on_in_skill_md(content: &str, always_on: bool) -> Option<String> {
    let rest = content.strip_prefix("---\n")?;
    let (fm, body) = rest.split_once("\n---")?;
    let mut lines: Vec<&str> = fm.lines().collect();
    let flag = if always_on {
        "always-on: true"
    } else {
        "always-on: false"
    };
    if let Some(pos) = lines
        .iter()
        .position(|l| l.trim_start().starts_with("always-on:"))
    {
        lines[pos] = flag;
    } else {
        lines.insert(0, flag);
    }
    Some(format!("---\n{}\n---{}", lines.join("\n"), body))
}

impl HostRuntime {
    pub fn list_skills(&self) -> Vec<types::SkillInfo> {
        self.skill_runtime.list_skills()
    }

    // ── WI-115: hooks script store (protocol/GUI management surface) ──

    pub fn list_hooks(&self) -> Vec<crate::hooks::HookScriptInfo> {
        crate::hooks::list_hook_scripts()
    }

    pub fn read_hook(&self, event: String, name: String) -> anyhow::Result<String> {
        crate::hooks::read_hook_script(&event, &name)
    }

    pub fn write_hook(&self, event: String, name: String, content: String) -> anyhow::Result<()> {
        crate::hooks::write_hook_script(&event, &name, &content)
    }

    pub fn delete_hook(&self, event: String, name: String) -> anyhow::Result<()> {
        crate::hooks::delete_hook_script(&event, &name)
    }

    pub fn scan_skills(&self) -> Vec<types::SkillInfo> {
        self.skill_runtime.scan_default_dirs();
        self.skill_runtime.list_skills()
    }

    pub fn create_skill(
        &self,
        name: String,
        description: String,
        command: Option<String>,
        body: String,
    ) -> anyhow::Result<Vec<types::SkillInfo>> {
        let content = build_skill_md(&name, &description, command.as_deref(), &body)
            .map_err(|e| anyhow::anyhow!(e))?;
        let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
        let dir = PathBuf::from(home)
            .join(".starriver")
            .join("skills")
            .join(&name);
        std::fs::create_dir_all(&dir).map_err(|e| anyhow::anyhow!("create skill dir: {e}"))?;
        std::fs::write(dir.join("SKILL.md"), content)
            .map_err(|e| anyhow::anyhow!("write SKILL.md: {e}"))?;
        self.skill_runtime.scan_default_dirs();
        Ok(self.skill_runtime.list_skills())
    }

    pub fn update_skill(
        &self,
        name: String,
        description: String,
        command: Option<String>,
        body: String,
    ) -> anyhow::Result<Vec<types::SkillInfo>> {
        let content = build_skill_md(&name, &description, command.as_deref(), &body)
            .map_err(|e| anyhow::anyhow!(e))?;
        let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
        let dir = PathBuf::from(home)
            .join(".starriver")
            .join("skills")
            .join(&name);
        if !dir.exists() {
            return Err(anyhow::anyhow!("skill not found: {name}"));
        }
        std::fs::write(dir.join("SKILL.md"), content)
            .map_err(|e| anyhow::anyhow!("write SKILL.md: {e}"))?;
        self.skill_runtime.scan_default_dirs();
        Ok(self.skill_runtime.list_skills())
    }

    #[api_method(
        "skills.delete",
        permissions = ["skills:write"],
        result = "crate::protocol_json::skill_info_list"
    )]
    pub fn delete_skill(&self, name: String) -> anyhow::Result<Vec<types::SkillInfo>> {
        // The name is a path component under the skills root — reject
        // traversal instead of remove_dir_all'ing outside it.
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        anyhow::ensure!(valid, "skill name must be [A-Za-z0-9_-]");
        let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
        let dir = PathBuf::from(home)
            .join(".starriver")
            .join("skills")
            .join(&name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| anyhow::anyhow!("delete skill: {e}"))?;
        }
        self.skill_runtime.scan_default_dirs();
        Ok(self.skill_runtime.list_skills())
    }

    pub fn set_skill_always_on(
        &self,
        name: String,
        always_on: bool,
    ) -> anyhow::Result<Vec<types::SkillInfo>> {
        // Same traversal guard as create/update/delete: the name is a path
        // component under the skills root.
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        anyhow::ensure!(valid, "skill name must be [A-Za-z0-9_-]");
        let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
        let dir = PathBuf::from(home)
            .join(".starriver")
            .join("skills")
            .join(&name);
        let path = dir.join("SKILL.md");
        let content =
            std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("read SKILL.md: {e}"))?;
        let updated = set_always_on_in_skill_md(&content, always_on)
            .ok_or_else(|| anyhow::anyhow!("invalid SKILL.md frontmatter"))?;
        std::fs::write(&path, updated).map_err(|e| anyhow::anyhow!("write SKILL.md: {e}"))?;
        self.skill_runtime.scan_default_dirs();
        let handle = self.runtime_handle.clone();
        let inner = self.inner.clone();
        let sk = name.clone();
        handle.spawn(async move {
            if let Some(runtime) = inner.lock().await.as_ref() {
                let _ = runtime
                    .host_store
                    .record_skill_usage(None, None, &sk, None, "always-on");
            }
        });
        // The always-on set feeds the kernel's skills resident, which
        // re-reads it on every assembly — nothing to invalidate beyond the
        // rescan above.
        Ok(self.skill_runtime.list_skills())
    }

    pub fn compact_session(
        &self,
        session_id: String,
        keep_recent: Option<u32>,
    ) -> anyhow::Result<types::CompactResult> {
        // A2a closeout: manual compaction runs through the kernel agent
        // (summarization + surface replace) — the legacy message-table trim
        // retired with the store. Cold sessions compact automatically at
        // their next turn boundary.
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        let (before, after) = handle.block_on(async {
            let l = inner.lock().await;
            let runtime = l
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("runtime not initialized"))?;
            let agent = runtime
                .agent
                .registry()
                .get(&starriver_kernel::session::SessionId(session_id.clone()))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "session is not running; compaction happens automatically at turn boundaries"
                    )
                })?;
            // The live settings document is authoritative (hot-seam: a
            // write takes effect on the next execute even when save() to
            // disk failed) — re-reading settings.yaml from disk ignores
            // hot updates and silently defaults on a corrupt file.
            let window = self
                .settings
                .read()
                .unwrap()
                .agent
                .context_window
                .unwrap_or(65_536);
            // `keepRecent` (protocol session.compact / UI /compact N) caps
            // the verbatim tail — honor it instead of silently running a
            // full-threshold compaction.
            match keep_recent {
                Some(n) => agent.force_compact_keep_recent(None, window, n).await,
                None => agent.force_compact(None, window).await,
            }
                .ok_or_else(|| anyhow::anyhow!("compaction did not run (session too small or summarize failed)"))
        })?;
        Ok(types::CompactResult {
            before: before as u32,
            after: after as u32,
        })
    }

    pub fn list_skill_usage(&self, limit: Option<u32>) -> Vec<types::SkillUsageRecord> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        let limit_val = limit.unwrap_or(100) as usize;
        handle
            .block_on(async {
                let l = inner.lock().await;
                match l.as_ref() {
                    Some(runtime) => runtime.host_store.list_skill_usage(limit_val),
                    None => vec![],
                }
            })
            .into_iter()
            .map(|r| types::SkillUsageRecord {
                session_id: r.session_id,
                project_id: r.project_id,
                skill: r.skill,
                version: r.version,
                mode: r.mode,
                ts: r.ts,
            })
            .collect()
    }

    pub fn list_plugins(&self) -> Vec<types::PluginEntry> {
        crate::plugin::PluginRegistry::new()
            .map(|reg| {
                reg.list()
                    .iter()
                    .map(|p| types::PluginEntry {
                        name: p.name.clone(),
                        description: p.description.clone(),
                        version: p.version.clone(),
                        skills: p.skills.clone(),
                        ui: p.ui.clone().map(|ui| types::PluginUiEntry {
                            entry: ui.entry,
                            tabs: ui
                                .tabs
                                .into_iter()
                                .map(|t| types::PluginUiTab {
                                    id: t.id,
                                    title: t.title,
                                })
                                .collect(),
                            permissions: ui.permissions,
                            theme: ui.theme,
                        }),
                        source_url: p.source_url.clone(),
                        installed_at: p.installed_at.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn install_plugin(&self, source_url: String) -> anyhow::Result<types::PluginEntry> {
        let entry = crate::plugin::install_plugin(&source_url).map_err(|e| anyhow::anyhow!(e))?;
        self.skill_runtime.scan_default_dirs();
        Ok(types::PluginEntry {
            name: entry.name,
            description: entry.description,
            version: entry.version,
            skills: entry.skills,
            ui: entry.ui.map(|ui| types::PluginUiEntry {
                entry: ui.entry,
                tabs: ui
                    .tabs
                    .into_iter()
                    .map(|t| types::PluginUiTab {
                        id: t.id,
                        title: t.title,
                    })
                    .collect(),
                permissions: ui.permissions,
                theme: ui.theme,
            }),
            source_url: entry.source_url,
            installed_at: entry.installed_at,
        })
    }

    pub fn uninstall_plugin(&self, name: String) -> anyhow::Result<Vec<types::PluginEntry>> {
        crate::plugin::uninstall_plugin(&name).map_err(|e| anyhow::anyhow!(e))?;
        self.skill_runtime.scan_default_dirs();
        Ok(self.list_plugins())
    }
}
