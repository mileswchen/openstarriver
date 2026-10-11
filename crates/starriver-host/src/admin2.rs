use crate::model_registry::ModelRegistry;
use starriver_api_macro::api_method;
use starriver_memory::{slugify, GLOBAL_PROJECT};

use crate::settings::{LlmProviderSettings, LlmVariantSettings};
use crate::{types, HostRuntime};

impl HostRuntime {
    pub fn list_models(&self) -> Vec<types::ModelEntry> {
        self.settings
            .read()
            .unwrap()
            .llm
            .providers
            .iter()
            .map(|e| types::ModelEntry {
                id: e.id.clone(),
                name: e.name.clone(),
                model_type: e.model_type.clone(),
                provider_type: e.provider_type.clone(),
                base_url: e.base_url.clone(),
                api_key: e.api_key.clone(),
                model_path: e.model_path.clone(),
                models: e
                    .models
                    .iter()
                    .map(|v| types::ModelVariant {
                        id: v.id.clone(),
                        name: v.name.clone(),
                    })
                    .collect(),
                default_model: e.default_model.clone(),
                concurrency: e.concurrency.map(|c| c as u32),
                max_tokens: e.max_tokens,
                context_window: e.context_window.map(|c| c as u32),
                temperature: e.temperature.map(|t| t as f64),
                api_format: e.api_format.clone(),
                reasoning_effort: e.reasoning_effort.clone(),
                model_label: e.model_label.clone(),
                supports_vision: e.supports_vision,
                vision_supported: crate::vision::effective_vision_support(&e.id),
            })
            .collect()
    }

    /// Set the execution mode (executionMode): writes sandboxMode/approvalMode per the
    /// mode profile into settings, affecting the default sandbox/approval for newly created
    /// sessions. Plan mode automatically appends the read-only lock.
    /// An empty string is equivalent to "standard" (the UI dropdown's Standard option sends
    /// null/empty) — otherwise switching back to Standard could not clear the persisted
    /// plan/ptc/goal state, and the kernel sandbox state would diverge from the UI.
    #[api_method("session.setExecutionMode", permissions = ["session:write"])]
    pub fn set_execution_mode(&self, mode: String) -> anyhow::Result<()> {
        use crate::execution_mode::ExecutionMode;
        let mode = if mode.is_empty() {
            "standard".to_string()
        } else {
            mode
        };
        let em = ExecutionMode::parse(&mode)
            .ok_or_else(|| anyhow::anyhow!("unknown execution mode `{mode}`"))?;
        // Governance note: no execution mode presets a ByPass approval or a
        // DangerFullAccess sandbox today, so no preset gate fires here. The
        // danger preset's high-risk gate lives at the UI confirmation layer.
        let mut settings = self.settings.write().unwrap();
        settings.llm.execution_mode = mode;
        drop(settings);
        // Persist to disk (same settings pathway as add_model etc.).
        let s = self.settings.read().unwrap();
        s.save()?;
        drop(s);
        // Push to the kernel: preset immediately switches to the new mode's defaults (Plan also enables the strict lock).
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        std::thread::spawn(move || {
            handle.block_on(async {
                let l = inner.lock().await;
                if let Some(ref runtime) = *l {
                    runtime.agent.apply_execution_mode(Some(em));
                }
            })
        })
        .join()
        .ok();
        Ok(())
    }

    /// Current execution mode. Without a session id: the deployment default
    /// (settings.yaml — the initial staged value for new sessions). With a
 /// session id: that session's creation-locked mode (binding), also
    /// replayed onto the kernel — after a restart or a session switch the
    /// sandbox/approval presets follow the session instead of sticking to the
    /// previous one. Empty strings normalize to "standard"; sessions created
    /// before the binding also fall back to standard without touching the kernel.
    pub fn get_execution_mode(&self, session_id: Option<String>) -> String {
        if let Some(sid) = session_id {
            let stored = {
                let inner = self.inner.clone();
                let handle = self.runtime_handle.clone();
                handle.block_on(async {
                    let l = inner.lock().await;
                    l.as_ref()
                        .map(|runtime| runtime.host_store.execution_mode_of(&sid))
                })
            };
            let Some(stored) = stored.flatten() else {
                return "standard".to_string();
            };
            let mode = if stored.is_empty() {
                "standard".to_string()
            } else {
                stored
            };
            if let Some(em) = crate::execution_mode::ExecutionMode::parse(&mode) {
                let inner = self.inner.clone();
                let handle = self.runtime_handle.clone();
                handle.block_on(async {
                    let l = inner.lock().await;
                    if let Some(ref runtime) = *l {
                        runtime.agent.apply_execution_mode(Some(em));
                    }
                });
            }
            return mode;
        }
        let m = self.settings.read().unwrap().llm.execution_mode.clone();
        if m.is_empty() {
            "standard".to_string()
        } else {
            m
        }
    }

    /// Protocol read side of the execution-mode binding. The Desktop UI calls
    /// the NAPI method directly; remote fronts (paired iPhone) only speak the
    /// protocol, and its session header renders the session's current mode.
    #[api_method("session.getExecutionMode", permissions = ["session:read"])]
    pub fn get_execution_mode_api(&self, session_id: Option<String>) -> anyhow::Result<String> {
        Ok(self.get_execution_mode(session_id))
    }

    /// Current settings document as JSON (backlog #7 settings seam).
    pub fn get_settings_json(&self) -> serde_json::Value {
        serde_json::to_value(&*self.settings.read().unwrap()).unwrap_or(serde_json::Value::Null)
    }

    /// Hot settings update (backlog #7): deep-merge the patch into the live
    /// settings, validate by deserializing back into [`Settings`], swap in
    /// place, and persist to settings.yaml. Consumers read the values at
    /// their point of use — the coding preset is re-derived per run, so a
    /// write takes effect on the next execute without a restart. Invalid
    /// patches are rejected wholesale.
    pub fn update_settings(&self, patch: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let updated = {
            let current = self.settings.read().unwrap();
            current.update_from_patch(&patch)?
        };
        {
            let mut live = self.settings.write().unwrap();
            *live = updated.clone();
        }
        if let Err(e) = updated.save() {
            // The in-memory value is already swapped (write-first wins);
            // persistence failure is surfaced but not rolled back.
            tracing::warn!(error = %e, "settings persisted failed after update");
        }
        serde_json::to_value(&updated).map_err(|e| anyhow::anyhow!("encode settings: {e}"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_model(
        &self,
        id: String,
        name: String,
        model_type: String,
        provider_type: String,
        base_url: Option<String>,
        api_key: Option<String>,
        model_path: Option<String>,
        default_model: String,
        variants_json: Option<String>,
        concurrency: Option<u32>,
        max_tokens: Option<u32>,
        context_window: Option<u32>,
        temperature: Option<f64>,
        api_format: Option<String>,
        reasoning_effort: Option<String>,
        model_label: Option<String>,
        supports_vision: Option<bool>,
    ) -> anyhow::Result<()> {
        // BUG-12: malformed variants_json is an error, not a silently empty
        // model list — one typo used to wipe every variant on re-save. An
        // empty/whitespace string still means "no variants" (the legacy napi
        // shell passes "" for absent values).
        let models: Vec<LlmVariantSettings> = match variants_json.as_deref() {
            Some(json) if !json.trim().is_empty() => serde_json::from_str(json)
                .map_err(|e| anyhow::anyhow!("invalid variants_json: {e}"))?,
            _ => vec![],
        };
        let raw_key = api_key.unwrap_or_default();
        let api_key_ref = if raw_key.is_empty() {
            // No new key supplied — preserve the existing reference (if any)
            // instead of wiping it, so re-saving a model form without re-entering
            // the key keeps the stored credential.
            self.settings
                .read()
                .unwrap()
                .llm
                .providers
                .iter()
                .find(|p| p.id == id)
                .map(|e| e.api_key.clone())
                .unwrap_or_default()
        } else {
            ModelRegistry::store_api_key(&id, &raw_key)
                .map_err(|e| anyhow::anyhow!("store key: {e}"))?
        };
        let entry = LlmProviderSettings {
            id,
            name,
            model_type,
            provider_type,
            base_url: base_url.unwrap_or_default(),
            api_key: api_key_ref,
            model_path,
            models,
            default_model,
            concurrency: concurrency.map(|c| c as usize),
            max_tokens,
            context_window: context_window.map(|c| c as usize),
            temperature: temperature.map(|t| t as f32),
            api_format: api_format.unwrap_or_else(|| "openai-chat".to_string()),
            reasoning_effort,
            // A blank custom ID is "unset" — the selector prefix falls back
            // to the display name instead of rendering an empty "· name".
            model_label: model_label
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty()),
            supports_vision,
        };
        {
            let mut settings = self.settings.write().unwrap();
            // In-place update on the same id — a replace must not reorder the
            // registry (list order is the display + default-pick order, and
            // memory routing falls back to providers.first()).
            if let Some(slot) = settings
                .llm
                .providers
                .iter_mut()
                .find(|p| p.id == entry.id)
            {
                *slot = entry;
            } else {
                settings.llm.providers.push(entry);
            }
            settings
                .save()
                .map_err(|e| anyhow::anyhow!("settings: {e}"))?;
        }
        // LLP §4.4: re-sync the provider directory / discovery so the new
        // route is visible to the configuration surface immediately.
        self.register_llm_directory();
        Ok(())
    }

    #[api_method("models.remove", permissions = ["models:write"])]
    pub fn remove_model(&self, id: String) -> anyhow::Result<()> {
        ModelRegistry::delete_api_key(&id);
        {
            let mut settings = self.settings.write().unwrap();
            settings.llm.providers.retain(|p| p.id != id);
            settings
                .save()
                .map_err(|e| anyhow::anyhow!("settings: {e}"))?;
        }
        self.register_llm_directory();
        Ok(())
    }

    pub fn list_agent_model_bindings(&self) -> Vec<types::AgentModelBinding> {
        let settings = self.settings.read().unwrap();
        self.hub
            .list()
            .iter()
            .map(|info| {
                let profile = self.hub.get(&info.name);
                let active = profile
                    .as_ref()
                    .and_then(|p| p.model.as_ref())
                    .map(|m| m.default.clone())
                    .unwrap_or_default();
                let mut bound_models: Vec<types::BoundModelInfo> = Vec::new();
                for entry in &settings.llm.providers {
                    // The custom model ID replaces the full display name as
                    // the selector prefix (blank = keep the display name).
                    let prefix = entry
                        .model_label
                        .as_deref()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .unwrap_or(&entry.name);
                    if entry.models.is_empty() {
                        let key = format!("{}/{}", entry.id, entry.default_model);
                        bound_models.push(types::BoundModelInfo {
                            id: key.clone(),
                            name: format!("{} · {}", prefix, entry.default_model),
                            enabled: true,
                        });
                    } else {
                        for v in &entry.models {
                            let key = format!("{}/{}", entry.id, v.id);
                            bound_models.push(types::BoundModelInfo {
                                id: key.clone(),
                                name: format!("{} · {}", prefix, v.name),
                                enabled: true,
                            });
                        }
                    }
                }
                types::AgentModelBinding {
                    agent_name: info.name.clone(),
                    active_model: active,
                    bound_models,
                    reasoning_effort: self
                        .reasoning_overrides
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&info.name)
                        .cloned(),
                }
            })
            .collect()
    }

    pub fn set_agent_active_model(
        &self,
        agent_name: String,
        provider_id: String,
        variant_id: String,
    ) -> anyhow::Result<()> {
        let (entry, model_name) = {
            let settings = self.settings.read().unwrap();
            let entry = settings
                .llm
                .providers
                .iter()
                .find(|p| p.id == provider_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Provider not found: {provider_id}"))?;
            let resolved = entry.resolve_variant(&variant_id);
            let model_name = resolved
                .map(|v| v.id.clone())
                .unwrap_or(entry.default_model.clone());
            (entry, model_name)
        };
        let default_val = format!("{}/{}", provider_id, variant_id);
        if let Some(info) = self.hub.list().into_iter().find(|a| a.name == agent_name) {
            // BUG-11: read errors abort (never overwrite through a failed
            // read) and write errors propagate; NotFound creates the skeleton.
            write_agent_md_model(&info.path, &agent_name, &default_val)?;
            if let Err(e) = self.hub.scan() {
                tracing::warn!(error = %e, "agent hub rescan after model update failed");
            }
        }
        if let Err(e) = self.activate_agent_model(&agent_name, &entry, &model_name) {
            tracing::warn!(agent = %agent_name, provider = %provider_id, model = %model_name, error = %e,
                "set_agent_active_model: engine activation failed (will retry on next execute)");
        }
        // WI-103: a turn of this agent may still be running — record the
        // switch so its `agent/request` rewrite adopts the new route on the
        // next LLM call instead of waiting for the next execute.
        {
            let inner = self.inner.blocking_lock();
            if let Some(runtime) = inner.as_ref() {
                runtime
                    .agent
                    .record_desired_route(&agent_name, &provider_id, &model_name);
            }
        }
        Ok(())
    }

    /// Session-scoped reasoning effort override for one agent (`""` = follow
    /// the provider's configured effort, `"off"` disables thinking). Re-routes
    /// the active model immediately so the next turn picks the level up.
    pub fn set_agent_reasoning_effort(
        &self,
        agent_name: String,
        effort: String,
    ) -> anyhow::Result<()> {
        let effort = effort.trim();
        match effort {
            "" | "off" | "low" | "high" | "max" => {}
            other => anyhow::bail!("invalid reasoning effort: `{other}` (off|low|high|max)"),
        }
        // The model to re-route — the agent's profile default, resolved after
        // the override table update below (the override affects the effort,
        // not the route target).
        let config = self.resolve_agent_config(&agent_name);
        let active = (config.model_id, config.model);
        {
            let mut map = self
                .reasoning_overrides
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if effort.is_empty() {
                map.remove(&agent_name);
            } else {
                map.insert(agent_name.clone(), effort.to_string());
            }
        }
        // Re-route immediately; failure is non-fatal (next execute re-activates).
        if let (Some(pid), model) = active {
            let entry = {
                let settings = self.settings.read().unwrap();
                settings.llm.providers.iter().find(|p| p.id == pid).cloned()
            };
            if let Some(entry) = entry {
                let model_name = model.unwrap_or_else(|| entry.default_model.clone());
                if let Err(e) = self.activate_agent_model(&agent_name, &entry, &model_name) {
                    tracing::warn!(agent = %agent_name, provider = %pid, error = %e,
                        "set_agent_reasoning_effort: engine re-route failed (will retry on next execute)");
                }
            }
        }
        Ok(())
    }

    pub fn market_list(&self) -> Vec<types::MarketEntry> {
        crate::market::MarketRegistry::load()
            .map(|reg| {
                reg.list()
                    .iter()
                    .map(|e| types::MarketEntry {
                        name: e.name.clone(),
                        description: e.description.clone(),
                        url: e.url.clone(),
                        entry_type: e.entry_type.clone(),
                        category: e.category.clone(),
                        version: e.version.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn list_memories(&self, project_id: Option<String>) -> Vec<types::MemoryEntry> {
        let pid = project_id.unwrap_or_else(|| GLOBAL_PROJECT.to_string());
        self.memory_store
            .list(&pid)
            .unwrap_or_default()
            .into_iter()
            .map(|e| types::MemoryEntry {
                name: e.name,
                description: e.description,
                body: e.body,
                source: e.source.as_str().to_string(),
                source_agent: e.source_agent,
                project: e.project,
                created: e.created as f64,
                updated: e.updated as f64,
            })
            .collect()
    }

    pub fn delete_memory(&self, project_id: String, name: String) -> anyhow::Result<()> {
        self.memory_store
            .delete(&project_id, &name)
            .map_err(|e| anyhow::anyhow!("delete memory: {e}"))
    }

    pub fn clear_memories(&self, project_id: String) -> anyhow::Result<u32> {
        self.memory_store
            .clear(&project_id)
            .map(|n| n as u32)
            .map_err(|e| anyhow::anyhow!("clear memories: {e}"))
    }

    /// Manual write from the memory panel (protocol `memory.upsert`): the
    /// name is slugified, same-name-different-body forks with a hash suffix
    /// instead of silently overwriting (WI-53), and the entry is badged
    /// `manual` via source_agent so the panel can show it apart from
    /// extraction-produced facts.
    pub fn upsert_memory(
        &self,
        project_id: Option<String>,
        name: String,
        description: String,
        body: String,
    ) -> anyhow::Result<types::MemoryEntry> {
        let pid = project_id.unwrap_or_else(|| GLOBAL_PROJECT.to_string());
        let now = starriver_memory::now_unix();
        let entry = starriver_memory::MemoryEntry {
            name: slugify(&name),
            description,
            body,
            created: now,
            updated: now,
            source: starriver_memory::MemorySource::Extract,
            source_agent: "manual".into(),
            project: pid,
        };
        let stored = self
            .memory_store
            .upsert_disambiguated(entry)
            .map_err(|e| anyhow::anyhow!("upsert memory: {e}"))?;
        Ok(types::MemoryEntry {
            name: stored.name,
            description: stored.description,
            body: stored.body,
            source: stored.source.as_str().to_string(),
            source_agent: stored.source_agent,
            project: stored.project,
            created: stored.created as f64,
            updated: stored.updated as f64,
        })
    }

    /// Recall preview for the memory panel (protocol `memory.recall`): what
    /// the injection path would pick for `query` in `project_id` — same
    /// scoring as the run path, capped by the store's top_k.
    pub fn recall_memories(
        &self,
        project_id: Option<String>,
        query: String,
    ) -> Vec<types::MemoryEntry> {
        let pid = project_id.unwrap_or_else(|| GLOBAL_PROJECT.to_string());
        self.memory_store
            .recall(&pid, &query, starriver_memory::now_unix())
            .into_iter()
            .map(|e| types::MemoryEntry {
                name: e.name,
                description: e.description,
                body: e.body,
                source: e.source.as_str().to_string(),
                source_agent: e.source_agent,
                project: e.project,
                created: e.created as f64,
                updated: e.updated as f64,
            })
            .collect()
    }

    /// Manual external-agent sweep (protocol `memory.ingest`).
    pub fn ingest_memories(&self) -> serde_json::Value {
        crate::memory_pipeline::run_ingestion(self)
    }

    /// Shared-store inventory (protocol `memory.status`).
    pub fn memory_status(&self) -> serde_json::Value {
        crate::memory_pipeline::status_json(self)
    }

    /// Instruction-file export (protocol `memory.export`).
    pub fn export_memories(&self, path: String) -> anyhow::Result<serde_json::Value> {
        crate::memory_pipeline::export_for_path(self, &path)
    }
}

// ── AGENT.md model pinning (BUG-11) ──

/// Rewrite an existing AGENT.md so its frontmatter pins `key` as
/// `model.default` (and lists it under `model.models`, deduplicated). The
/// body and unrelated frontmatter keys survive verbatim. Returns None when
/// the document's frontmatter does not parse — the caller must refuse to
/// rewrite rather than clobber the agent's file.
fn rewrite_agent_md_model(content: &str, key: &str) -> Option<String> {
    let doc = starriver_skills::profile::parser::parse_document(content).ok()?;
    let mut fm = doc.frontmatter.clone();
    let model_val = fm
        .entry("model".to_string())
        .or_insert(serde_yaml::Value::Mapping(Default::default()));
    // A string-valued `model: provider/x` (a form the profile parser
    // tolerates) must be replaced with a mapping — the old guard skipped the
    // pinning while still returning Some, so the file was rewritten as a
    // no-op and the active-model binding silently reverted on next load.
    if !matches!(model_val, serde_yaml::Value::Mapping(_)) {
        *model_val = serde_yaml::Value::Mapping(Default::default());
    }
    if let serde_yaml::Value::Mapping(ref mut map) = model_val {
        map.insert(
            serde_yaml::Value::String("default".into()),
            serde_yaml::Value::String(key.into()),
        );
        let models_entry = map
            .entry(serde_yaml::Value::String("models".into()))
            .or_insert(serde_yaml::Value::Sequence(vec![]));
        if let serde_yaml::Value::Sequence(ref mut seq) = models_entry {
            if !seq.iter().any(|v| v.as_str() == Some(key)) {
                seq.push(serde_yaml::Value::String(key.into()));
            }
        }
    }
    let yaml_str = serde_yaml::to_string(&fm).ok()?;
    let clean_yaml = yaml_str
        .trim()
        .trim_start_matches("---\n")
        .trim_end_matches("\n---")
        .trim_end_matches("\n...");
    Some(format!("---\n{}\n---\n\n{}", clean_yaml, doc.body.trim()))
}

/// Skeleton AGENT.md for an agent that has no file on disk yet.
fn new_agent_md(agent_name: &str, key: &str) -> String {
    format!(
        "---\nname: \"{}\"\nmodel:\n  default: \"{}\"\n---\n\n# {}\n",
        agent_name, key, agent_name
    )
}

/// Filesystem leg of `set_agent_active_model` (BUG-11): a missing file
/// creates the skeleton; any other read error aborts (an EACCES must never
/// fall through into the "create fresh" branch and overwrite the agent's
/// existing configuration); parse failures and write failures propagate.
fn write_agent_md_model(dir: &std::path::Path, agent_name: &str, key: &str) -> anyhow::Result<()> {
    let agent_md = dir.join("AGENT.md");
    match std::fs::read_to_string(&agent_md) {
        Ok(content) => {
            let new_content = rewrite_agent_md_model(&content, key).ok_or_else(|| {
                anyhow::anyhow!(
                    "AGENT.md frontmatter for `{agent_name}` does not parse; refusing to rewrite"
                )
            })?;
            std::fs::write(&agent_md, new_content)
                .map_err(|e| anyhow::anyhow!("write AGENT.md: {e}"))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir).map_err(|e| anyhow::anyhow!("create agent dir: {e}"))?;
            std::fs::write(&agent_md, new_agent_md(agent_name, key))
                .map_err(|e| anyhow::anyhow!("write AGENT.md: {e}"))?;
        }
        Err(e) => return Err(anyhow::anyhow!("read AGENT.md: {e}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WI-152 S1: the built-in vision table fills undeclared providers —
    /// deepseek resolves to vision-capable, moonshot to text-only, unknown
    /// custom endpoints default to supported. The raw UI declaration stays
    /// `None` (only the resolved field flips).
    #[test]
    fn builtin_vision_table_fills_undeclared_providers() {
        crate::testutil::with_temp_home(|_home| {
            let host = crate::HostRuntime::new(None).expect("host boots");
            for (id, base_url, expected) in [
                ("deepseek", "https://api.deepseek.com", true),
                ("moonshot", "https://api.moonshot.cn/v1", false),
            ] {
                host.add_model(
                    id.into(),
                    id.into(),
                    "cloud".into(),
                    "openai-compat".into(),
                    Some(base_url.into()),
                    Some("sk-test-key".into()),
                    None,
                    format!("{id}-chat"),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .expect("add model");
                assert_eq!(
                    host.active_model_supports_vision_for(id, &format!("{id}-chat")),
                    Some(expected),
                    "{id} runtime resolution"
                );
                let listed = host
                    .list_models()
                    .into_iter()
                    .find(|m| m.id == id)
                    .unwrap_or_else(|| panic!("{id} listed"));
                assert_eq!(listed.vision_supported, expected, "{id} resolved DTO field");
                assert_eq!(listed.supports_vision, None, "{id} raw declaration stays None");
            }
            // Undeclared + unknown provider → treated as supported (None).
            assert_eq!(
                host.active_model_supports_vision_for("custom-endpoint", ""),
                None
            );
        });
    }

    /// BUG-12: malformed variants_json is rejected and the provider is not
    /// persisted — one typo used to silently wipe the variant list.
    #[test]
    fn add_model_rejects_malformed_variants_json() {
        crate::testutil::with_temp_home(|_home| {
            let host = crate::HostRuntime::new(None).expect("host boots");
            let err = host
                .add_model(
                    "ds".into(),
                    "DeepSeek".into(),
                    "cloud".into(),
                    "openai-compat".into(),
                    Some("https://api.deepseek.com".into()),
                    Some("sk-test-key".into()),
                    None,
                    "deepseek-chat".into(),
                    Some("{broken".into()),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .expect_err("malformed variants_json");
            assert!(err.to_string().contains("variants_json"), "{err}");
            assert!(
                host.settings
                    .read()
                    .unwrap()
                    .llm
                    .providers
                    .iter()
                    .all(|p| p.id != "ds"),
                "rejected provider must not be persisted"
            );

            // Empty string still means "no variants" (legacy napi shell).
            host.add_model(
                "ds".into(),
                "DeepSeek".into(),
                "cloud".into(),
                "openai-compat".into(),
                Some("https://api.deepseek.com".into()),
                Some("sk-test-key".into()),
                None,
                "deepseek-chat".into(),
                Some("".into()),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .expect("empty variants_json is legal");
            assert!(
                host.settings
                    .read()
                    .unwrap()
                    .llm
                    .providers
                    .iter()
                    .any(|p| p.id == "ds"),
                "empty-variants provider persisted"
            );
        });
    }

    #[test]
    fn rewrite_pins_default_and_dedupes_models_list() {
        let base = "---\nname: reviewer\ntools:\n  - read\n---\n\nYou review code.\n";
        let out = rewrite_agent_md_model(base, "deepseek/deepseek-chat").unwrap();
        assert!(out.contains("default: deepseek/deepseek-chat"), "{out}");
        assert!(out.contains("models:\n  - deepseek/deepseek-chat"), "{out}");
        assert!(
            out.contains("- read"),
            "unrelated frontmatter survives: {out}"
        );
        assert!(out.contains("You review code."), "body survives: {out}");

        // Re-pinning the same key does not duplicate the models entry.
        let again = rewrite_agent_md_model(&out, "deepseek/deepseek-chat").unwrap();
        assert_eq!(
            again.matches("deepseek/deepseek-chat").count(),
            out.matches("deepseek/deepseek-chat").count(),
            "{again}"
        );

        // Unparseable frontmatter → None (caller refuses), never a clobber.
        assert_eq!(
            rewrite_agent_md_model("---\nname: [unclosed\n---\n\nbody\n", "k"),
            None
        );
    }

    #[test]
    fn write_creates_skeleton_rewrites_existing_propagates_errors() {
        let dir = tempfile::tempdir().unwrap();

        // Missing file → skeleton.
        write_agent_md_model(dir.path(), "coder", "ollama/qwen").unwrap();
        let created = std::fs::read_to_string(dir.path().join("AGENT.md")).unwrap();
        assert!(created.contains("name: \"coder\""), "{created}");
        assert!(created.contains("default: \"ollama/qwen\""), "{created}");

        // Existing file → frontmatter rewrite, body kept.
        std::fs::write(
            dir.path().join("AGENT.md"),
            "---\nname: coder\n---\n\nDo the work.\n",
        )
        .unwrap();
        write_agent_md_model(dir.path(), "coder", "openai/gpt-4o").unwrap();
        let updated = std::fs::read_to_string(dir.path().join("AGENT.md")).unwrap();
        assert!(updated.contains("openai/gpt-4o"), "{updated}");
        assert!(updated.contains("Do the work."), "{updated}");

        // Unparseable frontmatter → error, file untouched.
        let broken = "---\nname: [unclosed\n---\n\nbody\n";
        std::fs::write(dir.path().join("AGENT.md"), broken).unwrap();
        let err = write_agent_md_model(dir.path(), "coder", "openai/gpt-4o");
        assert!(err.is_err(), "malformed AGENT.md must not be rewritten");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("AGENT.md")).unwrap(),
            broken
        );

        // Read failure other than NotFound → error, never the create branch.
        // unix: an unreadable file (chmod is a no-op for root — probe first).
        #[cfg(unix)]
        {
            let unreadable = tempfile::tempdir().unwrap();
            let md = unreadable.path().join("AGENT.md");
            std::fs::write(&md, "---\nname: coder\n---\n\nbody\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&md, std::fs::Permissions::from_mode(0o000)).unwrap();
            let really_unreadable = std::fs::read_to_string(&md).is_err();
            if really_unreadable {
                let err = write_agent_md_model(unreadable.path(), "coder", "k/v");
                assert!(err.is_err(), "EACCES read must abort, not overwrite");
                assert!(std::fs::read_to_string(&md).is_err(), "file untouched");
            }
            let _ = std::fs::remove_file(&md);
        }
    }

    /// The custom model ID replaces the provider display name as the selector
    /// prefix; blank/whitespace falls back to the display name.
    #[test]
    fn binding_names_prefer_custom_model_label() {
        crate::testutil::with_temp_home(|_home| {
            let host = crate::HostRuntime::new(None).expect("host boots");
            let add = |id: &str, name: &str, label: Option<String>| {
                host.add_model(
                    id.into(),
                    name.into(),
                    "cloud".into(),
                    "openai-compat".into(),
                    Some("https://api.example.com".into()),
                    Some("sk-test-key".into()),
                    None,
                    format!("{id}-chat"),
                    Some(
                        serde_json::to_string(&[
                            serde_json::json!({ "id": format!("{id}-chat"), "name": "Chat" }),
                        ])
                        .unwrap(),
                    ),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    label,
                    None,
                )
                .expect("add model");
            };
            add("glm", "GLM Coding Plan - CN", Some("CN".into()));
            add("ds", "DeepSeek", Some("   ".into()));

            let names_of = |provider: &str| -> Vec<String> {
                host.list_agent_model_bindings()
                    .into_iter()
                    .next()
                    .map(|b| {
                        b.bound_models
                            .into_iter()
                            .filter(|m| m.id.starts_with(&format!("{provider}/")))
                            .map(|m| m.name)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            assert_eq!(names_of("glm"), vec!["CN · Chat".to_string()]);
            // Whitespace-only label = unset → display name.
            assert_eq!(names_of("ds"), vec!["DeepSeek · Chat".to_string()]);

            // Re-saving with no label clears it — back to the display name.
            add("glm", "GLM Coding Plan - CN", None);
            assert_eq!(
                names_of("glm"),
                vec!["GLM Coding Plan - CN · Chat".to_string()]
            );
        });
    }
}
