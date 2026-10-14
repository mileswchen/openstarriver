use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use futures::future::BoxFuture;
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use tokio::sync::Mutex;

use crate::model_registry::ModelRegistry;
use starriver_api_macro::api_method;
use starriver_kernel::config::RuntimeConfig;
use starriver_kernel::llm::{
    LlmConfigurableProvider, LlmDiscoveredModel, LlmError, LlmModelDiscovery,
    LlmModelDiscoveryRequest, LlmRuntime,
};
use starriver_kernel::tools::ToolRuntime;
use starriver_llm_anthropic::CredentialRef as AnthropicCredentialRef;
use starriver_llm_openai::{
    CredentialRef as OpenAiCredentialRef, OpenAiCompletionsAdapter, ProviderProfile,
};
use starriver_memory::{MemoryConfig, MemoryStore};
use starriver_skills::profile::agent::AgentProfile;
use starriver_skills::profile::types::ToolRef;

use crate::{
    helpers::apply_slash_dispatch,
    settings::{LlmProviderSettings, LlmVariantSettings, Settings},
    RuntimeInner, ScriptExecutor,
};

/// Shared execution runtime. Desktop forwards through a thin napi shell; TUI calls it directly.
pub struct HostRuntime {
    pub(crate) inner: Arc<Mutex<Option<RuntimeInner>>>,
    pub(crate) hub: Arc<starriver_skills::hub::AgentHub>,
    pub(crate) skill_runtime: Arc<crate::skill_runtime::SkillRuntime>,
    pub(crate) provider_registry: Arc<RwLock<crate::providers::ProviderRegistry>>,
    /// LLP §6: provider route declarations live in the `llm:` settings
    /// namespace; the legacy model registry only feeds the one-time migration.
    pub(crate) settings: Arc<RwLock<Settings>>,
    pub(crate) attachment_store: Arc<crate::attachments::AttachmentStore>,
    /// Effect handles for the provider directory / model-discovery
    /// registrations (kept alive for the runtime's lifetime).
    pub(crate) llm_directory_handles: Arc<StdMutex<Vec<cordis_rslite::EffectHandle>>>,
    pub(crate) mcp_config: Arc<RwLock<crate::mcp::config::McpConfig>>,
    pub(crate) mcp_tools: Arc<RwLock<std::collections::HashMap<String, Vec<String>>>>,
    pub(crate) scheduler: Arc<crate::scheduler::TaskScheduler>,
    pub(crate) script_executor: Arc<RwLock<Option<Arc<dyn ScriptExecutor>>>>,
    pub(crate) memory_store: Arc<MemoryStore>,
    /// Session-level meter for auto-memory LLM usage (WI-53): `from_env`
    /// once per process, shared by both extraction pipelines, read out by
    /// `memory.status`.
    pub(crate) memory_budget: Arc<starriver_memory::TokenBudget>,
    /// 逐字节实时一致（开放回合快照流）：per-session 投影世代
    /// （compaction/fork/回滚 +1；客户端 epoch 不一致即全量重拉）。
    pub(crate) trace_epochs: Arc<StdMutex<HashMap<String, u64>>>,
    pub(crate) runtime_handle: tokio::runtime::Handle,
    /// BUG-5: start_scheduler_tick idempotence guard.
    pub(crate) scheduler_tick_started: std::sync::Mutex<bool>,
    /// Per-session single input slot (FPM §6.3): the set of currently running
    /// sessions; a repeated execute returns SESSION_BUSY, serializing
    /// concurrent commands from multiple clients.
    pub(crate) running_sessions: Arc<StdMutex<std::collections::HashSet<String>>>,
    /// Agent-scoped reasoning effort overrides ("off"|"low"|"high"|"max").
    /// Session-scoped (not persisted): absent entry = follow the provider's
    /// configured effort. Routes are registered per provider, so two agents
    /// sharing one provider share the effective effort of whoever activated
    /// last — an accepted edge for the in-process runtime.
    pub(crate) reasoning_overrides: Arc<StdMutex<std::collections::HashMap<String, String>>>,
    /// Agent whose persona/skills currently occupy the kernel's residents.
    /// `execute` swaps them when a run names a different agent — the kernel
    /// keeps single-active-persona semantics, so this is the one piece of
    /// "which agent" state the host still holds, and it follows the runs.
    pub(crate) persona_agent: Arc<StdMutex<Option<String>>>,
    /// Registration handle for the `subagent_spawn` tool. Kept so
    /// `set_subagent_config` can dispose and re-install the definition — the
    /// model-facing description names the enabled templates, so an enable/disable
    /// must re-register (register rejects same-name duplicates).
    pub(crate) subagent_tool_handle: Arc<StdMutex<Option<cordis_rslite::EffectHandle>>>,
}

/// Session run guard: `execute` marks the session busy at the start, and the
/// guard's drop (task completion/cancellation/early exit) clears it
/// automatically, so the single input slot never leaks on error paths.
struct SessionRunGuard {
    running: Arc<StdMutex<std::collections::HashSet<String>>>,
    session_id: Option<String>,
}

impl Drop for SessionRunGuard {
    fn drop(&mut self) {
        if let Some(session_id) = &self.session_id {
            self.running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(session_id);
        }
    }
}

impl SessionRunGuard {
    /// Fresh-session execute: the caller sent no id, so construction had
    /// nothing to register and `sessions.running` stayed blind for the whole
    /// first turn — the sidebar's WI-179 poll keys on it to materialize the
    /// new row while the turn is live, so without adoption the row only
    /// showed up at turn end whenever the one-shot TaskStarted refresh
    /// missed. A freshly minted uuid cannot collide in the set.
    fn adopt(&mut self, session_id: &str) {
        if self.session_id.is_some() {
            return;
        }
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string());
        self.session_id = Some(session_id.to_string());
    }
}

impl HostRuntime {
    /// WI-48 feedback slice: record user 👍/👎 for a session. Durable
    /// (`feedback/record` event, live or cold) and visible (feedback
    /// resident slot injects recent lines into later requests).
    pub fn record_feedback(
        &self,
        session_id: String,
        kind: String,
        comment: Option<String>,
    ) -> anyhow::Result<()> {
        if !matches!(kind.as_str(), "up" | "down") {
            anyhow::bail!("kind must be 'up' or 'down', got: {kind}");
        }
        let mut entries = vec![(
            "kind".into(),
            starriver_kernel::json::JsonValue::String(kind.clone()),
        )];
        if let Some(comment) = comment.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            entries.push((
                "comment".into(),
                starriver_kernel::json::JsonValue::String(comment.to_string()),
            ));
        }
        let label = if kind == "up" { "👍" } else { "👎" };
        let mut line = format!("{label} {session_id}");
        if let Some(comment) = comment.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            line.push_str(&format!(" — {comment}"));
        }
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        handle.block_on(async {
            let l = inner.lock().await;
            let Some(runtime) = l.as_ref() else {
                anyhow::bail!("runtime not loaded");
            };
            crate::sessions::append_session_event(
                runtime,
                &session_id,
                "feedback/record",
                starriver_kernel::json::JsonValue::Object(entries),
            )
            .await?;
            runtime.agent.push_feedback_line(line);
            Ok(())
        })
    }

    pub fn new(config_path: Option<String>) -> anyhow::Result<Self> {
        // WI-163: file tracing must exist before the first MCP discovery
        // warn, or the failure is dropped again.
        crate::logging::init_host_tracing();
        let config = if let Some(ref path) = config_path {
            RuntimeConfig::load(Some(&PathBuf::from(path)))
                .map_err(|e| anyhow::anyhow!("config: {e}"))?
        } else {
            RuntimeConfig::default()
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!("tokio: {e}"))?;
        let runtime_handle = runtime.handle().clone();
        // The legacy registry only carries MCP bridge tools now; kernel
        // built-ins live on the starriver-tools seam.
        let mcp_tools: Arc<RwLock<std::collections::HashMap<String, Vec<String>>>> =
            Arc::new(RwLock::new(Default::default()));
        let plugin_bus = cordis_rslite::EventBus::new();
        let plugin_ctx = cordis_rslite::Context::new().with_event_bus(&plugin_bus);
        // WI-163: MCP availability state — servers whose tools are NOT
        // registered feed the per-run status notice (see plugin_kernel's
        // default_agent_options) instead of silently vanishing from the
        // model's vocabulary.
        let mcp_unavailable = Arc::new(crate::mcp_status_state::McpUnavailableServers::default());
        let _ = plugin_ctx.provide::<crate::mcp_status_state::McpUnavailableServers>(
            Arc::clone(&mcp_unavailable),
        );
        // Attachment seam: register the default content-addressed store so
        // loop-side consumers resolve it via `ctx.require` (WI-6 seam conversion).
        {
            let store = crate::attachments::open_attachment_store();
            let _ = plugin_ctx.provide::<starriver_kernel::attachments::AttachmentStore>(Arc::new(
                starriver_kernel::attachments::AttachmentStore::new(store),
            ));
        }
        let plugin_db_path = {
            // Plugin persistence lives in its own file until the legacy
            // SessionStore migrates; sharing one db file would collide on
            // schema ownership (STRR vs legacy tables).
            let mut path = std::path::PathBuf::from(&config.session.db_path);
            let file = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "sessions.db".into());
            path.set_file_name(format!("{file}.plugin.db"));
            path
        };
        let host_db_path = {
            let mut path = std::path::PathBuf::from(&config.session.db_path);
            let file = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "sessions.db".into());
            path.set_file_name(format!("{file}.host.db"));
            path
        };
        let host_store = crate::host_store::HostStore::open(host_db_path, plugin_bus.clone())?;
        let persistence = Arc::new(runtime.block_on(async {
            starriver_session_persistence::SqliteSessionPersistence::open_with_bus(
                plugin_db_path,
                plugin_bus.clone(),
            )
            .map_err(|e| anyhow::anyhow!("persistence db: {e}"))
        })?);
        let persistence_for_agent = Arc::clone(&persistence);
        let runtime_bus = plugin_bus.clone();
        let agent = Arc::new(runtime.block_on(async move {
            Ok::<_, anyhow::Error>(crate::plugin_kernel::PluginKernel::new(
                plugin_ctx,
                plugin_bus,
                persistence_for_agent,
            ))
        })?);
        let hub = Arc::new(starriver_skills::hub::AgentHub::with_defaults());
        let _ = hub.scan();
        // Backfill built-in agents by name: only create missing AGENT.md files, never overwrite user config.
        // (Previously used `hub.list().is_empty()` as a whole — after a rename, old installs still have
        // the old directory, causing the new name to be missing.)
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        let agents_dir = home.join(".starriver").join("agents");
        let agent_configs: &[(&str, &str, &str, &str, &str)] = &[
            ("Chat Agent", "General-purpose AI chat assistant",
             "tools: []\n", "mode: chat\n",
             "You are a helpful AI assistant. Answer questions directly using your knowledge. Do not call tools or run commands unless explicitly asked."),
            (crate::CODING_AGENT_NAME, "AI coding agent for code generation and review",
             "tools: [read, write, edit, bash, grep, lsp_symbols, lsp_diagnostics]\n",
             "mode: build\n",
             "You are a coding agent. You can read, edit, and create files, search the codebase, and execute shell commands. Always explain your changes."),
        ];
        for (name, desc, tools_yaml, mode_yaml, body) in agent_configs {
            let dir = agents_dir.join(name);
            if dir.join("AGENT.md").exists() {
                continue;
            }
            // WI-164: a failed backfill used to vanish silently — the
            // built-in agent would simply be missing from the hub.
            let md = format!("---\nname: \"{name}\"\nversion: \"0.1.0\"\ndescription: \"{desc}\"\n{tools_yaml}{mode_yaml}---\n\n# {name}\n\n{body}\n");
            if let Err(error) = std::fs::create_dir_all(&dir) {
                tracing::warn!(agent = %name, dir = %dir.display(), error = %error, "built-in agent dir creation failed; agent will be missing");
                continue;
            }
            if let Err(error) = std::fs::write(dir.join("AGENT.md"), md) {
                tracing::warn!(agent = %name, path = %dir.join("AGENT.md").display(), error = %error, "built-in AGENT.md write failed; agent will be missing");
            }
        }
        let _ = hub.scan();
        let skill_runtime = Arc::new(crate::skill_runtime::SkillRuntime::new());
        // Feed the kernel's `skills:always-on` resident from the host skill
        // loader; the resident re-reads it on every assembly.
        agent.attach_skill_manager(skill_runtime.manager());
        let provider_registry = Arc::new(RwLock::new(
            crate::providers::ProviderRegistry::new()
                .map_err(|e| anyhow::anyhow!("providers: {e}"))?,
        ));
        // LLP §6 settings slice: route declarations move into `llm.providers`
        // in settings.yaml. The legacy models.yaml (via ModelRegistry) is only
        // consulted once, as a migration source, then stops being a route
        // source. ModelRegistry::new() also migrates plaintext keys to the
        // secrets file as a side effect.
        let legacy_models = ModelRegistry::new().map_err(|e| anyhow::anyhow!("models: {e}"))?;
        let settings = Arc::new(RwLock::new(Settings::load()));
        // Push the execution mode to the kernel at startup (single source for the preset + Plan strict-lock decision).
        agent.apply_execution_mode(crate::execution_mode::ExecutionMode::parse(
            &settings.read().unwrap().llm.execution_mode,
        ));
        {
            let mut settings = settings.write().unwrap();
            if settings.llm.providers.is_empty() {
                let migrated = Self::migrate_models_to_settings(&legacy_models);
                if !migrated.is_empty() {
                    settings.llm.providers = migrated;
                    if let Err(e) = settings.save() {
                        tracing::warn!(error = %e, "failed to persist migrated llm settings");
                    }
                }
            }
        }
        let mcp_config = {
            let default_mcp = crate::mcp::config::McpConfig {
                mcp_servers: Default::default(),
            };
            if let Some(ref path) = crate::mcp::config::McpConfig::default_path() {
                if path.exists() {
                    crate::mcp::config::McpConfig::load(path).unwrap_or(default_mcp)
                } else {
                    default_mcp
                }
            } else {
                default_mcp
            }
        };
        let mcp_config = Arc::new(RwLock::new(mcp_config));
        // MCP discovery: one handshake per enabled server produces both the
        // legacy McpTools (napi/admin surface) and kernel ToolDefinitions (the
        // new agent-loop seam). Registered after the plugin kernel exists so
        // the same tool list reaches both registries.
        //
        // 2026-10-04: the handshakes run on the background runtime, never
        // inline. Inline discovery made HostRuntime::new — and with it the
        // Electron window, which is only created after the backend finishes
        // initializing — wait out the slowest server's connect timeout: a
        // sleeping LAN http endpoint burned its full ~75s TCP timeout while
        // the app sat in the dock looking dead (host.log 2026-10-04
        // 03:19:39Z, window ~90s after launch). Background discovery lands
        // through the same paths as the WI-163 retry loop: tools register as
        // each server answers, and slow or failed servers surface via the
        // unavailability notice instead of blocking first paint.
        let mcp_servers: Vec<(String, crate::mcp::config::McpServerConfig)> = mcp_config
            .read()
            .unwrap()
            .enabled_servers()
            .into_iter()
            .map(|(name, srv)| (name, srv.clone()))
            .collect();
        {
            let agent = Arc::clone(&agent);
            let mcp_tools = Arc::clone(&mcp_tools);
            let mcp_unavailable = Arc::clone(&mcp_unavailable);
            let runtime_bus = runtime_bus.clone();
            let handle = runtime.handle().clone();
            runtime.spawn(async move {
                for (name, srv) in mcp_servers {
                    let name_for_token = name.clone();
                    let token_provider: crate::mcp::adapter::TokenProvider =
                        Arc::new(move || crate::mcp::oauth::token(&name_for_token));
                    match crate::mcp::definition::discover_bridge_shared(
                        &name,
                        &srv,
                        token_provider.clone(),
                    )
                    .await
                    {
                        Ok((definitions, connection)) => {
                            reregister_discovered(
                                Arc::clone(&agent),
                                Arc::clone(&mcp_tools),
                                Arc::clone(&mcp_unavailable),
                                name.clone(),
                                definitions,
                            )
                            .await;
                            supervise_mcp(
                                Arc::clone(&agent),
                                Arc::clone(&mcp_tools),
                                Arc::clone(&mcp_unavailable),
                                runtime_bus.clone(),
                                handle.clone(),
                                name.clone(),
                                srv.clone(),
                                token_provider,
                                connection,
                            );
                        }
                        Err(e) => {
                            // WI-163: no more silent permanent skip — log, mark the
                            // server unavailable for the session-notice feed, tell
                            // the bus, and keep retrying in the background.
                            tracing::warn!(server = %name, error = %e, "MCP discovery failed; retrying in the background");
                            mcp_unavailable.mark(&name, format!("{e:#}"));
                            runtime_bus.emit(Arc::new(
                                crate::mcp::health::McpServerHealth {
                                    server: name.clone(),
                                    status: "retrying".into(),
                                    detail: format!("{e:#}"),
                                },
                            ));
                            spawn_discovery_retry(
                                Arc::clone(&agent),
                                Arc::clone(&mcp_tools),
                                Arc::clone(&mcp_unavailable),
                                runtime_bus.clone(),
                                handle.clone(),
                                name.clone(),
                                srv.clone(),
                                token_provider,
                            );
                        }
                    }
                }
            });
        }
        let scheduler_path = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".starriver")
            .join("scheduled_tasks.yaml");
        let scheduler = Arc::new(crate::scheduler::TaskScheduler::new(scheduler_path.clone()));
        // WI-164: a failed load used to be dropped — every scheduled task
        // silently dead with no trace. (Corrupt-file cases already log+back
        // up inside load(); this covers the rest, e.g. io errors.)
        if let Err(error) = runtime.block_on(async { scheduler.load().await }) {
            tracing::warn!(
                path = %scheduler_path.display(),
                error = %error,
                "scheduled tasks load failed; scheduler starts empty"
            );
        }
        let memory_root = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".starriver")
            .join("memory");
        // Cross-agent shared root (docs/37): one-time move off the legacy
        // starcode-scoped location; failure is silent and harmless.
        {
            let home = std::env::var("HOME").unwrap_or_default();
            starriver_memory::ingest::migrate_legacy_root(
                std::path::Path::new(&home),
                &memory_root,
            );
        }
        let memory_store = Arc::new(MemoryStore::new(memory_root, MemoryConfig::default()));
        let memory_budget = Arc::new(starriver_memory::TokenBudget::from_env());
        let trace_epochs = Arc::new(StdMutex::new(HashMap::new()));
        let runtime = Self {
            scheduler_tick_started: Default::default(),
            inner: Arc::new(Mutex::new(Some(RuntimeInner {
                goals: Default::default(),
                runtime,
                bus: runtime_bus,
                agent,
                persistence,
                host_store,
            }))),
            hub,
            provider_registry,
            settings,
            attachment_store: crate::attachments::open_attachment_store(),
            llm_directory_handles: Arc::new(StdMutex::new(Vec::new())),
            mcp_config,
            mcp_tools,
            scheduler,
            reasoning_overrides: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            persona_agent: Arc::new(StdMutex::new(None)),
            skill_runtime,
            script_executor: Arc::new(RwLock::new(None)),
            memory_store,
            memory_budget,
            trace_epochs,
            runtime_handle,
            running_sessions: Arc::new(StdMutex::new(std::collections::HashSet::new())),
            subagent_tool_handle: Arc::new(StdMutex::new(None)),
        };
        // LLP §4.4: declare every configured provider in the kernel directory
        // and wire discovery even before any route is activated.
        runtime.register_llm_directory();
 // subagent parity: the in-process subagent driver tool.
        runtime.install_subagent_tool();
        // WI-67 user-questions: the agent can ask the user mid-run.
        runtime.install_question_tool();
        // WI-102 image generation: provider images endpoint → attachment store.
        runtime.install_imagegen_tool();
        Ok(runtime)
    }

    /// Register the `generate_image` tool (WI-102): provider settings and
    /// the attachment store are sampled per call, so settings edits apply
    /// without re-registration; the definition itself is registered once
    /// here on the shared tool runtime.
    pub fn install_imagegen_tool(&self) {
        let settings = self.settings.clone();
        let store = Arc::clone(&self.attachment_store);
        let config: crate::tools::imagegen::ImageGenConfigSource = Arc::new(move || {
            let snapshot = settings.read().unwrap_or_else(|e| e.into_inner());
            crate::tools::imagegen::ImageGenConfig {
                providers: snapshot.llm.providers.clone(),
                image_model: snapshot.llm.image_model.clone(),
            }
        });
        let def = crate::tools::imagegen::generate_image_definition(config, store);
        if let Some(inner) = self.inner.blocking_lock().as_ref() {
            register_tool_or_warn(inner.agent.tools(), def, "generate_image");
        }
    }

    /// Register the `subagent_spawn` tool backed by an in-process driver:
    /// a fresh agent session runs the prompt under the template's bound model
    /// (settings `subagents:` namespace), or inherits the calling agent's
 /// route when the template binds none (parent-inheritance parity),
    /// and returns its final text. Only enabled templates are advertised in
    /// the model-facing description and accepted at spawn time.
    pub fn install_subagent_tool(&self) {
        let spawner = self.subagent_spawner();
        let settings = self.settings.clone();
        let enabled: crate::tools::subagent::EnabledTemplates = {
            let settings = Arc::clone(&settings);
            Arc::new(move || {
                settings
                    .read()
                    .unwrap()
                    .subagents
                    .entries
                    .iter()
                    .filter(|e| e.enabled)
                    .map(|e| e.name.clone())
                    .collect()
            })
        };
        let def = crate::tools::subagent::subagent_spawn_definition(spawner, enabled);
        if let Some(inner) = self.inner.blocking_lock().as_ref() {
            match inner.agent.tools().register(def) {
                Ok(handle) => *self.subagent_tool_handle.lock().unwrap() = Some(handle),
                Err(e) => tracing::warn!("subagent_spawn tool registration failed: {e}"),
            }
        }
    }

    /// Build the `subagent_spawn` driver: in-process sub-sessions for plain
    /// templates, external ACP child processes for `acp:`-declared ones
    /// (WI-112). Shared by the tool registration and the test hook below.
    fn subagent_spawner(&self) -> crate::tools::subagent::SubagentSpawner {
        let inner = self.inner.clone();
        let settings = self.settings.clone();
        let spawner: crate::tools::subagent::SubagentSpawner =
            Arc::new(move |template, prompt, caller_session| {
                let inner = Arc::clone(&inner);
                let settings = Arc::clone(&settings);
                Box::pin(async move {
 // Runtime gate mirrors the executor's check (two-layer
                    // parity): settings may change between schema read and spawn.
                    let (bound_model, acp_agent) = {
                        let s = settings.read().unwrap();
                        match s.subagents.entries.iter().find(|e| e.name == template) {
                            Some(e) if e.enabled => (e.model.clone(), e.acp.clone()),
                            _ => {
                                return Err(format!(
                                    "subagent template `{template}` is not enabled"
                                ))
                            }
                        }
                    };
                    // WI-112: an external ACP agent template runs the prompt
                    // in a child process (ACP over stdio) instead of an
                    // in-process sub-session; the child carries its own model.
                    if let Some(agent) = acp_agent {
                        let spec = starriver_acp_client::AcpAgentSpec {
                            command: agent.command.clone(),
                            args: agent.args.clone(),
                            cwd: None,
                            permissions: if agent.trust {
                                starriver_acp_client::PermissionPolicy::Allow
                            } else {
                                starriver_acp_client::PermissionPolicy::Reject
                            },
                        };
                        return tokio::task::spawn_blocking(move || {
                            starriver_acp_client::run_prompt(
                                &spec,
                                &prompt,
                                std::time::Duration::from_secs(600),
                            )
                            .map(|result| {
                                format!(
                                    "{} (stopReason: {})",
                                    result.text.trim(), result.stop_reason
                                )
                            })
                            .map_err(|e| e.to_string())
                        })
                        .await
                        .map_err(|e| format!("acp subagent task failed: {e}"))?;
                    }
                    let kernel = {
                        let l = inner.lock().await;
                        l.as_ref().map(|i| i.agent.clone())
                    };
                    let Some(kernel) = kernel else {
                        return Err("runtime unavailable".into());
                    };
 // parent-inheritance parity: an unbound template runs
                    // the child on the calling agent's live route instead of
                    // the preset placeholder.
                    let inherited = if bound_model.is_none() {
                        caller_session.and_then(|sid| {
                            use starriver_kernel::agent::AgentRegistry as _;
                            let agent = kernel
                                .registry()
                                .get(&starriver_kernel::session::SessionId(sid))?;
                            let options = agent.options();
                            match (options.provider.clone(), options.model.clone()) {
                                (Some(p), Some(m)) if !p.is_empty() && !m.is_empty() => {
                                    Some((p, m))
                                }
                                _ => None,
                            }
                        })
                    } else {
                        None
                    };
                    kernel
                        .spawn_subagent(prompt, bound_model, inherited)
                        .await
                        .map_err(|e| e.to_string())
                })
            });
        spawner
    }

    /// Test hook: drive the same spawner the `subagent_spawn` tool uses.
    #[doc(hidden)]
    pub fn debug_spawn_subagent(
        self: &std::sync::Arc<Self>,
        template: &str,
        prompt: &str,
    ) -> Result<String, String> {
        let spawner = self.subagent_spawner();
        let runtime = self.runtime_handle().clone();
        let (template, prompt) = (template.to_string(), prompt.to_string());
        runtime.block_on(async move { spawner(template, prompt, None).await })
    }


    /// Re-register `subagent_spawn` after a delegation-config change: the
    /// model-facing description names the enabled templates, so the old
    /// definition must be disposed before the fresh one registers.
    pub fn reinstall_subagent_tool(&self) {
        let old = self.subagent_tool_handle.lock().unwrap().take();
        if let Some(handle) = old {
            self.runtime_handle.block_on(handle.dispose_async());
        }
        self.install_subagent_tool();
    }

    /// Test observation hook: the model-facing description of the registered
    /// `subagent_spawn` tool (`None` when it is not registered).
    #[doc(hidden)]
    pub fn debug_subagent_tool_description(&self) -> Option<String> {
        let inner = self.inner.blocking_lock();
        let runtime = inner.as_ref()?;
        let schema = runtime
            .agent
            .tools()
            .schemas()
            .into_iter()
            .find(|s| s.name == "subagent_spawn")?;
        Some(schema.description)
    }

    /// Register the `ask_user` tool backed by the host question bridge: the
    /// question surfaces as a UiEvent modal on desktop; the answer (or the
    /// timeout) comes back as the tool's content.
    pub fn install_question_tool(&self) {
        let bridge = {
            let inner = self.inner.blocking_lock();
            match inner.as_ref() {
                Some(inner) => Arc::clone(&inner.agent.question_bridge),
                None => return,
            }
        };
        let scheduler = Arc::clone(&self.scheduler);
        let asker: crate::tools::question::UserAsker = Arc::new(
            move |session_id: String,
                  question: String,
                  options: Vec<String>,
                  timeout: Duration|
                  -> BoxFuture<'static, Result<String, String>> {
                let bridge = Arc::clone(&bridge);
                let scheduler = Arc::clone(&scheduler);
                Box::pin(async move {
                    // Unattended guard: a session owned by a live scheduled
                    // run has no user on the other side — fail the ask
                    // immediately instead of stalling for the full timeout
                    // (the tool maps the miss to "decide autonomously").
                    if scheduler
                        .list_active_runs()
                        .iter()
                        .any(|run| run.session_id == session_id)
                    {
                        return Err("unavailable".to_string());
                    }
                    match bridge.ask(&session_id, &question, &options, timeout).await {
                        Ok(answer) => Ok(answer),
                        Err(e) => Err(e.as_str().to_string()),
                    }
                })
            },
        );
        let def = crate::tools::question::ask_user_definition(asker);
        if let Some(inner) = self.inner.blocking_lock().as_ref() {
            register_tool_or_warn(inner.agent.tools(), def, "ask_user");
        }
    }

    /// UI-channel answer for a pending question (protocol
    /// `interaction.respond`). `false` = no such pending question.
    pub fn respond_question(&self, question_id: &str, answer: &str) -> bool {
        let bridge = {
            let inner = self.inner.blocking_lock();
            match inner.as_ref() {
                Some(inner) => Arc::clone(&inner.agent.question_bridge),
                None => return false,
            }
        };
        bridge.respond(question_id, answer)
    }

    /// One-time migration: legacy `models.yaml` entries become `llm.providers`
    /// route declarations in settings.yaml (LLP §6). Credential references
    /// (`!file:` / `${ENV}`) carry over untouched.
    fn migrate_models_to_settings(legacy: &ModelRegistry) -> Vec<LlmProviderSettings> {
        legacy
            .list()
            .iter()
            .map(|e| LlmProviderSettings {
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
                    .map(|v| LlmVariantSettings {
                        id: v.id.clone(),
                        name: v.name.clone(),
                    })
                    .collect(),
                default_model: e.default_model.clone(),
                concurrency: e.concurrency,
                max_tokens: e.max_tokens,
                context_window: e.context_window,
                temperature: e.temperature,
                api_format: e.api_format.clone(),
                reasoning_effort: e.reasoning_effort.clone(),
                model_label: None,
                supports_vision: None,
            })
            .collect()
    }

    /// For the napi thin shell/client to spawn relay tasks (consumed by execute's per-run receiver).
    pub fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.runtime_handle.clone()
    }

    /// Single-input-slot query: whether the session currently has a running
    /// task (fast-path check for the protocol `session.execute`; `execute`
    /// still falls back to an atomic insert internally).
    pub fn is_session_running(&self, session_id: &str) -> bool {
        self.running_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(session_id)
    }

    /// Profile `tools:` rendered to runtime tool names (`script:`/`mcp_` prefixed).
    pub fn profile_tool_names(profile: &AgentProfile) -> Vec<String> {
        profile
            .tools
            .iter()
            .map(|t| match t {
                ToolRef::Builtin(name) => name.clone(),
                ToolRef::Script(name) => format!("script:{}", name),
                ToolRef::Mcp(name) => format!("mcp_{}", name),
            })
            .collect()
    }

    /// Effective reasoning effort override for `agent_name` (`None` inside the
    /// Some = explicitly off; no Some = follow the provider configuration).
    pub(crate) fn agent_reasoning_override(&self, agent_name: &str) -> Option<Option<String>> {
        let map = self
            .reasoning_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.get(agent_name)
            .map(|e| if e == "off" { None } else { Some(e.clone()) })
    }

    /// Activate `entry`/`model_name` with the agent's reasoning override applied.
    pub(crate) fn activate_agent_model(
        &self,
        agent_name: &str,
        entry: &LlmProviderSettings,
        model_name: &str,
    ) -> anyhow::Result<()> {
        let mut entry = entry.clone();
        if let Some(ovr) = self.agent_reasoning_override(agent_name) {
            entry.reasoning_effort = ovr;
        }
        self.resolve_and_activate_model(&entry, model_name)
    }

    pub(crate) fn resolve_and_activate_model(
        &self,
        entry: &LlmProviderSettings,
        model_name: &str,
    ) -> anyhow::Result<()> {
        eprintln!(
            "[rust] resolve_and_activate_model: type={} provider={} base_url={} model={}",
            entry.model_type, entry.provider_type, entry.base_url, model_name
        );
        let gen_cfg = entry.generation_config();
        let reasoning_effort = gen_cfg.reasoning_effort.clone();
        validate_api_format(&gen_cfg.api_format)
            .map_err(|e| anyhow::anyhow!("model `{}`: {e}", entry.id))?;
        let wire_api = Self::wire_api_for(&gen_cfg.api_format);
        // LLP D-C: the credential reference (`!file:` / `${ENV}` / literal) is
        // passed through to the adapter un-resolved; the host resolver does
        // the per-request lookup so secrets are never frozen into routes.
        let credential = Self::credential_ref_from(&entry.api_key);
        if gen_cfg.api_format == "anthropic" {
            self.register_anthropic_route(&entry.id, &entry.base_url, &entry.api_key);
            return Ok(());
        }
        match (entry.model_type.as_str(), entry.provider_type.as_str()) {
            ("cloud", _) | (_, "openai-compat") => {
                self.register_openai_route(
                    &entry.id,
                    &entry.base_url,
                    credential,
                    reasoning_effort.clone(),
                    wire_api,
                    Self::openai_model_entries(entry),
                );
            }
            ("local", "ollama") => {
                self.register_ollama_route(&entry.id, &entry.base_url);
            }
            ("local", "lmstudio") => {
                // lmstudio speaks openai-completions: reuse the openai route,
                // prefilling the model directory from /models (restores the
                // legacy discovery behavior).
                let probe_profile = Self::openai_profile_fn(
                    entry.id.clone(),
                    entry.base_url.clone(),
                    credential.clone(),
                    "openai-completions".into(),
                    reasoning_effort.clone(),
                    Vec::new(),
                );
                let probe = OpenAiCompletionsAdapter::new(entry.id.clone(), probe_profile)
                    .with_credential_resolver(Self::credential_resolver());
                let models = self
                    .runtime_handle
                    .block_on(probe.detect_models())
                    .unwrap_or_default();
                self.register_openai_route(
                    &entry.id,
                    &entry.base_url,
                    credential,
                    reasoning_effort.clone(),
                    "openai-completions",
                    models,
                );
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "unsupported engine: type={} provider={}",
                    entry.model_type,
                    entry.provider_type
                ))
            }
        }
        Ok(())
    }

    pub(crate) fn register_openai_route(
        &self,
        provider_id: &str,
        base_url: &str,
        credential: Option<OpenAiCredentialRef>,
        reasoning_effort: Option<String>,
        api: &str,
        models: Vec<starriver_llm_openai::ModelEntry>,
    ) {
        let profile_fn = Self::openai_profile_fn(
            provider_id.to_string(),
            base_url.to_string(),
            credential,
            api.to_string(),
            reasoning_effort,
            models,
        );
        if let Some(inner) = self.inner.blocking_lock().as_ref() {
            let adapter = OpenAiCompletionsAdapter::new(provider_id.to_string(), profile_fn)
                .with_credential_resolver(Self::credential_resolver());
            inner.agent.register_provider_adapter(Arc::new(adapter));
        }
    }

    fn openai_profile_fn(
        provider: String,
        base_url: String,
        credential: Option<OpenAiCredentialRef>,
        api: String,
        reasoning_effort: Option<String>,
        models: Vec<starriver_llm_openai::ModelEntry>,
    ) -> Arc<dyn Fn() -> starriver_llm_openai::ProviderProfile + Send + Sync> {
        Arc::new(move || starriver_llm_openai::ProviderProfile {
            provider: provider.clone(),
            api: api.clone(),
            base_url: base_url.clone(),
            credential: credential.clone(),
            models: models.clone(),
            retry_policy: None,
            idle_timeout_ms: 300_000,
            max_tokens: None,
            thinking: None,
            reasoning_effort: reasoning_effort.clone(),
        })
    }

    /// Map the model config's wire format to the adapter profile `api`.
    /// "openai-responses" routes gpt-5/o-series models through the responses
    /// API; everything else (and locals) stays on chat completions.
    fn wire_api_for(api_format: &str) -> &'static str {
        match api_format {
            "openai-responses" => "openai-responses",
            _ => "openai-completions",
        }
    }

    fn register_anthropic_route(&self, provider_id: &str, base_url: &str, api_key: &str) {
        let credential = Self::anthropic_credential_ref(api_key);
        let api_key_field = api_key.to_string();
        let resolver: starriver_llm_anthropic::CredentialResolver =
            Arc::new(move || Self::resolve_credential_field(&api_key_field));
        let inner = self.inner.blocking_lock();
        if let Some(inner) = inner.as_ref() {
            let adapter = starriver_llm_anthropic::AnthropicAdapter::new(
                provider_id.to_string(),
                base_url.to_string(),
                credential,
            )
            .with_credential_resolver(resolver);
            inner.agent.register_provider_adapter(Arc::new(adapter));
        }
    }

    fn register_ollama_route(&self, provider_id: &str, base_url: &str) {
        let inner = self.inner.blocking_lock();
        if let Some(inner) = inner.as_ref() {
            let adapter = starriver_llm_ollama::OllamaAdapter::new(
                provider_id.to_string(),
                base_url.to_string(),
            );
            inner.agent.register_provider_adapter(Arc::new(adapter));
        }
    }

    // ── Credential references (LLP D-C) ────────────────────────────────

    /// Map a stored `api_key` field to a [`CredentialRef`] without resolving
    /// it: `!file:<account>` → secrets account, `${ENV}` → env var, anything
    /// else (legacy literal) → Literal. Empty fields are `None`.
    pub(crate) fn credential_ref_from(field: &str) -> Option<OpenAiCredentialRef> {
        if field.is_empty() {
            return None;
        }
        if let Some(account) = field.strip_prefix("!file:") {
            Some(OpenAiCredentialRef::File(account.to_string()))
        } else if let Some(var) = field.strip_prefix("${").and_then(|s| s.strip_suffix("}")) {
            Some(OpenAiCredentialRef::Env(var.to_string()))
        } else {
            Some(OpenAiCredentialRef::Literal(field.to_string()))
        }
    }

    fn anthropic_credential_ref(field: &str) -> Option<AnthropicCredentialRef> {
        if field.is_empty() {
            return None;
        }
        if let Some(account) = field.strip_prefix("!file:") {
            Some(AnthropicCredentialRef::File(account.to_string()))
        } else if let Some(var) = field.strip_prefix("${").and_then(|s| s.strip_suffix("}")) {
            Some(AnthropicCredentialRef::Env(var.to_string()))
        } else {
            Some(AnthropicCredentialRef::Literal(field.to_string()))
        }
    }

    /// Resolve one stored credential field through the shared secrets layer
    /// (`!file:` / `!keychain:` / `${ENV}` / literal). Failures normalize to
    /// `MISSING_CREDENTIAL` so adapters never see a half-resolved key.
    fn resolve_credential_field(field: &str) -> Result<String, LlmError> {
        if field.is_empty() {
            return Err(Self::missing_credential("route (empty key)"));
        }
        starriver_secrets::credentials::CredentialsStore::new()
            .resolve_field(field)
            .map_err(|e| Self::missing_credential(&format!("{field}: {e}")))
    }

    /// Host-owned resolver for OpenAI-family adapters: reads the credential
    /// reference off each request's profile snapshot and resolves it against
    /// the secrets store / env — never a frozen literal.
    fn credential_resolver() -> starriver_llm_openai::CredentialResolver {
        Arc::new(|profile: &ProviderProfile| {
            let field = match profile.credential.as_ref() {
                Some(OpenAiCredentialRef::Literal(key)) => key.clone(),
                Some(OpenAiCredentialRef::Env(name)) => format!("${{{name}}}"),
                Some(OpenAiCredentialRef::File(account)) => format!("!file:{account}"),
                None => {
                    return Err(Self::missing_credential("route (no credential)"));
                }
            };
            Self::resolve_credential_field(&field)
        })
    }

    fn missing_credential(source: &str) -> LlmError {
        LlmError::Call {
            code: "MISSING_CREDENTIAL".into(),
            message: format!("credential unavailable: {source}"),
            status: None,
            provider_retry_after_ms: None,
            request_id: None,
        }
    }

    fn openai_model_entries(entry: &LlmProviderSettings) -> Vec<starriver_llm_openai::ModelEntry> {
        entry
            .models
            .iter()
            .map(|v| starriver_llm_openai::ModelEntry {
                id: v.id.clone(),
                name: Some(v.name.clone()),
                context_window: None,
                max_tokens: None,
            })
            .collect()
    }

    // ── Provider directory / dormant mounts (LLP §4.4) ─────────────────

    /// Register every configured provider into the kernel's configurable
    /// provider directory and wire up model discovery, so the configuration
 /// surface is visible even before a route is activated (: directory is
    /// the registry twin; zero routes still declare the catalog). Re-syncs are
    /// idempotent: previous directory/discovery handles are disposed first, so
    /// add/remove-model changes take effect without a restart.
    pub fn register_llm_directory(&self) {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return;
        };
        {
            let drained: Vec<_> = self
                .llm_directory_handles
                .lock()
                .unwrap()
                .drain(..)
                .collect();
            for handle in drained {
                self.runtime_handle.block_on(handle.dispose_async());
            }
        }
        let llm = inner.agent.llm();
        let providers = self.settings.read().unwrap().llm.providers.clone();
        // Declaration source: configured providers plus the built-in cloud
        // presets (dormant mounts). A provider the user removed stays
        // declared but unconfigured, so the configuration face exists before
        // any route is activated (LLP §4.4).
        let mut entries: Vec<LlmConfigurableProvider> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for p in &providers {
            seen.insert(p.id.clone());
            entries.push(LlmConfigurableProvider {
                provider: p.id.clone(),
                display_name: p.name.clone(),
                settings_ns: format!("llm.providers.{}", p.id),
                settings_path: vec!["llm".into(), "providers".into(), p.id.clone()],
                declared: Some(true),
            });
        }
        for preset in crate::helpers::builtin_provider_presets(|_| false) {
            if seen.insert(preset.id.clone()) {
                entries.push(LlmConfigurableProvider {
                    provider: preset.id.clone(),
                    display_name: preset.name.clone(),
                    settings_ns: format!("llm.providers.{}", preset.id),
                    settings_path: vec!["llm".into(), "providers".into(), preset.id.clone()],
                    declared: Some(true),
                });
            }
        }
        if entries.is_empty() {
            return;
        }
        if let Ok(handle) = llm.register_configurable_providers(entries) {
            self.llm_directory_handles.lock().unwrap().push(handle);
        }
        for provider in providers {
            let settings_ns = format!("llm.providers.{}", provider.id);
            if let Ok(handle) =
                llm.register_model_discovery(settings_ns, Self::model_discovery_for(&provider))
            {
                self.llm_directory_handles.lock().unwrap().push(handle);
            }
        }
    }

    fn model_discovery_for(provider: &LlmProviderSettings) -> LlmModelDiscovery {
        let provider = provider.clone();
        match provider.provider_type.as_str() {
            "openai-compat" | "lmstudio" => {
                let resolver = Self::credential_resolver();
                Arc::new(move |request: LlmModelDiscoveryRequest| {
                    let base_url = request
                        .base_url
                        .clone()
                        .unwrap_or_else(|| provider.base_url.clone());
                    let api = request
                        .api
                        .clone()
                        .unwrap_or_else(|| "openai-completions".into());
                    let provider_id = provider.id.clone();
                    let provider_api_key = provider.api_key.clone();
                    let provider_reasoning = provider.reasoning_effort.clone();
                    let provider_id_for_adapter = provider_id.clone();
                    let profile = Arc::new(move || ProviderProfile {
                        provider: provider_id.clone(),
                        api: api.clone(),
                        base_url: base_url.clone(),
                        credential: Self::credential_ref_from(&provider_api_key),
                        models: Vec::new(),
                        retry_policy: None,
                        idle_timeout_ms: 300_000,
                        max_tokens: None,
                        thinking: None,
                        reasoning_effort: provider_reasoning.clone(),
                    });
                    let adapter = OpenAiCompletionsAdapter::new(provider_id_for_adapter, profile)
                        .with_credential_resolver(resolver.clone());
                    Box::pin(async move {
                        let models = adapter.detect_models().await?;
                        Ok(models
                            .into_iter()
                            .map(|m| LlmDiscoveredModel {
                                id: m.id,
                                name: m.name,
                                context_window: m.context_window,
                                max_tokens: m.max_tokens,
                            })
                            .collect())
                    })
                })
            }
            "ollama" => Arc::new(move |request: LlmModelDiscoveryRequest| {
                let base_url = request
                    .base_url
                    .unwrap_or_else(|| provider.base_url.clone());
                let adapter =
                    starriver_llm_ollama::OllamaAdapter::new(provider.id.clone(), base_url);
                Box::pin(async move {
                    let models = adapter.detect_models().await?;
                    Ok(models
                        .into_iter()
                        .map(|m| LlmDiscoveredModel {
                            id: m.id,
                            name: m.name,
                            context_window: m.context_window,
                            max_tokens: m.max_tokens,
                        })
                        .collect())
                })
            }),
            _ => Arc::new(|_| Box::pin(async { Ok(Vec::new()) })),
        }
    }

    // ── Test observation hooks ─────────────────────────────────────────

    #[doc(hidden)]
    pub fn debug_configurable_providers(&self) -> Vec<String> {
        let inner = self.inner.blocking_lock();
        inner
            .as_ref()
            .map(|inner| {
                inner
                    .agent
                    .llm()
                    .list_configurable_providers()
                    .into_iter()
                    .map(|p| p.provider)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[doc(hidden)]
    pub fn debug_model_discovery(&self, settings_ns: String) -> anyhow::Result<usize> {
        let inner = self.inner.blocking_lock();
        let Some(inner) = inner.as_ref() else {
            return Err(anyhow::anyhow!("runtime unavailable"));
        };
        let llm = inner.agent.llm();
        let handle = self.runtime_handle.clone();
        handle
            .block_on(async move {
                llm.discover_models(
                    settings_ns,
                    LlmModelDiscoveryRequest {
                        provider: Some("probe".into()),
                        base_url: Some("http://127.0.0.1:1".into()),
                        api: None,
                        api_key: None,
                        cancellation: None,
                    },
                )
                .await
                .map(|models| models.len())
            })
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    #[allow(unused_assignments)]
    /// In-process memory of "this provider+model route rejects images",
    /// written by probing the error keywords of an image-sending turn
    /// (WI-152 S4: keyed per route and TTL-bounded; only consulted for routes
    /// with NO vision declaration — a declared route is never flipped by a
    /// runtime error, and the UI's supports_vision checkbox always wins).
    fn text_only_providers(
    ) -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::SystemTime>> {
        static SET: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<String, std::time::SystemTime>>,
        > = std::sync::OnceLock::new();
        SET.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    /// Memory key for one provider+model route (WI-152 S4 granularity).
    fn vision_memory_key(provider: &str, model: &str) -> String {
        format!("{provider}/{model}")
    }

    /// Whether a recorded memory still counts (WI-152 S4 TTL; `0` = forever).
    fn memory_entry_current(
        recorded: std::time::SystemTime,
        ttl_hours: u64,
        now: std::time::SystemTime,
    ) -> bool {
        ttl_hours == 0
            || now
                .duration_since(recorded)
                .map(|age| age < std::time::Duration::from_secs(ttl_hours * 3600))
                .unwrap_or(true)
    }

    fn active_provider_key(&self) -> String {
        self.debug_default_provider()
            .map(|p| p.to_string())
            .unwrap_or_else(|| "mock".into())
    }

    /// Same lookup for an explicit provider+model route (the run's activated
 /// route). Fully automatic (WI-152 v2 — /`inputModalities`
    /// semantics, no user setting):
    /// 1. the built-in provider table decides for known providers;
    /// 2. unknown providers default to supported, with the runtime error
    ///    memory (TTL-bounded, provider+model keyed) as the only corrective.
    pub fn active_model_supports_vision_for(&self, key: &str, model: &str) -> Option<bool> {
        let ttl_hours = self
            .settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .agent
            .vision_error_memory_ttl_hours
            .unwrap_or(168);
        // WI-152: the built-in provider table decides and is never flipped by
        // runtime errors.
        if let Some(builtin) = crate::vision::builtin_vision(key) {
            return Some(builtin);
        }
        // Undeclared custom endpoint: consult (and expire) the error memory.
        let memory_key = Self::vision_memory_key(key, model);
        let mut memory = Self::text_only_providers()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match memory.get(&memory_key).copied() {
            Some(recorded)
                if Self::memory_entry_current(
                    recorded,
                    ttl_hours,
                    std::time::SystemTime::now(),
                ) =>
            {
                return Some(false);
            }
            Some(_) => {
                memory.remove(&memory_key);
            }
            None => {}
        }
        None
    }

    /// The current model's image capability. `Some(false)`: declaration, the
    /// built-in table, or (undeclared routes only) unexpired error memory
    /// says text-only; `Some(true)`/`None`: treat as supported.
    pub fn active_model_supports_vision(&self) -> Option<bool> {
        let key = self.active_provider_key();
        self.active_model_supports_vision_for(&key, "")
    }

    #[api_method("session.execute", permissions = ["session:execute"], streaming = true)]
    #[allow(clippy::too_many_arguments)]
    pub fn execute(
        &self,
        task: String,
        session_id: Option<String>,
        execution_mode: Option<String>,
        agent: Option<String>,
        isolation: Option<bool>,
        agent_preset: Option<String>,
        permission_preset: Option<String>,
    ) -> anyhow::Result<tokio::sync::broadcast::Receiver<serde_json::Value>> {
 // Staged execution mode for a NEW session (binding: chosen at
        // creation, locked per session). Only an explicitly passed value takes
        // effect (the Coding renderer always sends one for new sessions,
        // including standard; Chat sends none and the kernel mode stays
        // untouched); continuing sessions ignore the parameter — the mode
        // persists with the session and session.getExecutionMode(sessionId)
        // replays it onto the kernel on load.
        let fresh_mode: Option<crate::execution_mode::ExecutionMode> =
            match (&session_id, &execution_mode) {
                (None, Some(requested)) => {
                    let em = crate::execution_mode::ExecutionMode::parse(requested)
                        .ok_or_else(|| anyhow::anyhow!("unknown execution mode `{requested}`"))?;
                    // Governance note: no execution mode presets a ByPass approval
                    // or a DangerFullAccess sandbox today, so no preset gate fires
                    // here (the danger preset's high-risk gate lives at the UI
                    // confirmation layer).
                    Some(em)
                }
                _ => None,
            };
        // BUG-113 creation-time permission binding: the composer's pre-send
        // pick rides execute and is stamped into the fresh session before the
        // first turn runs. Unknown preset names fail loudly (the UI surfaces
        // protocol errors) instead of silently leaving the session on the
        // kernel default while advertising the pick. The kernel ignores the
        // bundle on resume turns — mid-session switches go through
        // session.setPermissionPreset.
        let permission_bundle = match &permission_preset {
            Some(name) => {
                let p = crate::permission_presets::parse(name).ok_or_else(|| {
                    anyhow::anyhow!("unknown permission preset `{name}` (manual|accept-edits|auto|danger-full-access)")
                })?;
                Some(crate::preset::Preset { sandbox: p.sandbox, approval: p.approval })
            }
            None => None,
        };
        // Single input slot: a task is already running for the same session →
        // SESSION_BUSY (mapped at the protocol layer).
        // The guard clears automatically when the spawned task ends (including
        // cancellation/early-exit paths).
        let running_guard = {
            let running = Arc::clone(&self.running_sessions);
            if let Some(sid) = &session_id {
                let mut set = running.lock().unwrap_or_else(|e| e.into_inner());
                if !set.insert(sid.clone()) {
                    return Err(anyhow::anyhow!(
                        "SESSION_BUSY: session `{sid}` is already running (single input slot)"
                    ));
                }
            }
            SessionRunGuard {
                running,
                session_id: session_id.clone(),
            }
        };
        // Which agent runs: named by the caller, defaulting to the built-in
        // Chat Agent. Its config is aggregated from the durable sources on
        // every run (profile / workspace activation / reasoning table), so a
        // model or workspace change lands on the very next turn. A named but
        // unknown agent is a caller bug — fail the run instead of silently
        // falling back.
        let agent_name = agent.unwrap_or_else(|| crate::CHAT_AGENT_NAME.to_string());
        self.ensure_agent_residency(&agent_name)?;
 // WI-5 (ui-agent-preset parity): agent_preset is a creation-time
        // binding like execution_mode. New session → validate against the
        // roster and thread to the kernel header stamp; continuing session →
        // locked (AGENT_PRESET_LOCKED). Every run re-syncs the registry so
        // resume-path resolution sees freshly authored presets.
        let validate_preset = |preset_id: &str| -> anyhow::Result<String> {
            let row = self
                .list_agent_presets()
                .into_iter()
                .find(|row| row.id == preset_id)
                .ok_or_else(|| {
                    anyhow::anyhow!("AGENT_PRESET_NOT_FOUND: unknown agent preset `{preset_id}`")
                })?;
            if let Some(reason) = row.broken {
                return Err(anyhow::anyhow!(
                    "AGENT_PRESET_BROKEN: agent preset `{preset_id}` is broken: {reason}"
                ));
            }
            Ok(preset_id.to_string())
        };
        let agent_preset = match (&session_id, &agent_preset) {
            (Some(sid), Some(preset_id)) => {
                // Callers may pre-choose the session id for a FRESH session,
                // so "continuing" is a durability question, not an Option
                // question: the binding locks once the session has a log.
                let exists = {
                    let inner = self.inner.blocking_lock();
                    match inner.as_ref() {
                        Some(runtime) => runtime
                            .persistence
                            .coordinator()
                            .load(starriver_kernel::session::SessionId(sid.clone()))
                            .is_ok(),
                        None => false,
                    }
                };
                if exists {
                    return Err(anyhow::anyhow!(
                        "AGENT_PRESET_LOCKED: the agent preset is locked to this session (chosen at creation)"
                    ));
                }
                Some(validate_preset(preset_id)?)
            }
            (None, Some(preset_id)) => Some(validate_preset(preset_id)?),
            _ => None,
        };
        self.sync_agent_presets();
        let config = self.resolve_agent_config(&agent_name);
        // WI-103/BUG-88: this run's route was just seeded from the profile —
        // drop a pending mid-run switch ONLY when it recorded the same route.
        // A switch that landed between the profile read above and this clear
        // (or one pending for another session of this agent) survives, so
        // the rewrite listener still applies it and the user's switch is not
        // silently lost for the turn.
        {
            let inner = self.inner.blocking_lock();
            if let Some(runtime) = inner.as_ref() {
                runtime.agent.clear_desired_route_if(
                    &agent_name,
                    config.model_id.as_deref(),
                    config.model.as_deref(),
                );
            }
        }
        let model_override = config.model.clone();
        let model_id = config.model_id.clone();
        let project_id = config.project_id.clone();
        let working_dir = config.working_dir.clone();
        // Isolation for this run: the caller's per-session toggle wins, the
        // agent profile's `isolate:` default otherwise.
        let isolate_worktree = isolation.unwrap_or(config.isolate_default);
        let memory_cfg = config.memory.clone();

        // Memory recall injection (docs/36 path B, restored in WI-66 / BUG-1):
        // for the memory-carrying agent, recall the facts relevant to this
        // task and hand them to the kernel as a trailing system section —
        // recall runs on the hot path so the injection reflects entries
        // written up to this very turn. `llm_select` profiles spend one
        // LLM call to pick the entry set; plain recall scores locally.
        let mut memory_injection: Option<String> = None;
        if memory_cfg.enabled && agent_name == crate::CODING_AGENT_NAME {
            let store = Arc::new(self.memory_store.with_config(memory_cfg.clone()));
            let pid = project_id
                .clone()
                .unwrap_or_else(|| starriver_memory::GLOBAL_PROJECT.to_string());
            let entries = if store.config().llm_select {
                let (provider, model) = {
                    let settings = self.settings.read().unwrap();
                    let provider_settings = model_id.as_ref().and_then(|mid| {
                        settings
                            .llm
                            .providers
                            .iter()
                            .find(|p| &p.id == mid)
                            .cloned()
                    });
                    match provider_settings {
                        Some(entry) => (
                            entry.id.clone(),
                            model_override
                                .clone()
                                .unwrap_or_else(|| entry.default_model.clone()),
                        ),
                        None => (String::new(), String::new()),
                    }
                };
                let llm = self
                    .inner
                    .blocking_lock()
                    .as_ref()
                    .map(|inner| inner.agent.llm());
                let handle = self.runtime_handle.clone();
                let pid2 = pid.clone();
                let query = task.clone();
                if let (Some(llm), false) = (llm, provider.is_empty()) {
                    let chat = {
                        let llm = Arc::clone(&llm);
                        let provider = provider.clone();
                        let model = model.clone();
                        move |prompt: String| -> futures::future::BoxFuture<
                            'static,
                            Result<String, String>,
                        > {
                            let llm = Arc::clone(&llm);
                            let provider = provider.clone();
                            let model = model.clone();
                            Box::pin(async move {
                                use starriver_kernel::llm::{
                                    ContentBlock, GenerateOptions, Message, MessageId,
                                    MessageRole, MessageSource,
                                };
                                let options = GenerateOptions {
                                    provider,
                                    model,
                                    reasoning_effort: None,
                                    tools: None,
                                    messages: vec![Message {
                                        id: MessageId("memory-select".into()),
                                        role: MessageRole::User,
                                        content: vec![ContentBlock::Text(prompt)],
                                        source: MessageSource::User,
                                    }],
                                    system: None,
                                    temperature: None,
                                    max_tokens: Some(1024),
                                    stop: None,
                                    session_id: None,
                                    agent_id: None,
                                    tool_choice: None,
                                    cancellation: None,
                                    purpose: None,
                                    loop_assembled: false,
                                };
                                llm.chat(options).await.map_err(|e| e.to_string())
                            })
                        }
                    };
                    handle.block_on(starriver_memory::llm_select(&store, &pid2, &query, chat))
                } else {
                    Vec::new()
                }
            } else {
                store.recall(&pid, &task, starriver_memory::now_unix())
            };
            if !entries.is_empty() {
                memory_injection = Some(starriver_memory::render_inject(
                    &entries,
                    store.config().inject_budget_tokens,
                ));
            }
        }
        // Point the kernel's dynamic memory resident at this run's facts (or
        // clear it — a stale section must not leak into other agents' runs).
        if let Some(kernel) = self.inner.blocking_lock().as_ref().map(|i| i.agent.clone()) {
            kernel.set_memory_injection(memory_injection);
        }

        let cmd: Option<String> = task
            .split_whitespace()
            .next()
            .filter(|t| t.starts_with('/'))
            .map(|s| s.to_string());
        let resolved = cmd
            .as_deref()
            .and_then(|c| self.skill_runtime.manager().resolve_by_command(c));
        // The skill's prompt injection has no carrier on this path (the kernel
        // assembles the system prompt from its residents); only the task
        // rewrite and the usage record are live.
        let (mut task, _injection) = apply_slash_dispatch(&task, resolved.as_ref());
        if let Some(skill) = &resolved {
            let handle = self.runtime_handle.clone();
            let inner = self.inner.clone();
            let sk = skill.name.clone();
            let ver = skill.version.clone();
            let pid = project_id.clone();
            handle.spawn(async move {
                if let Some(runtime) = inner.lock().await.as_ref() {
                    let _ = runtime.host_store.record_skill_usage(
                        None,
                        pid.as_deref(),
                        &sk,
                        ver.as_deref(),
                        "command",
                    );
                }
            });
        }

        let mut title_provider = String::new();
        let mut title_model = String::new();
        if let Some(ref mid) = model_id {
            let settings = self.settings.read().unwrap();
            let entry = settings
                .llm
                .providers
                .iter()
                .find(|p| &p.id == mid)
                .cloned();
            if let Some(entry) = entry {
                let mname = model_override.as_deref().unwrap_or(&entry.default_model);
                self.activate_agent_model(&agent_name, &entry, mname)?;
                title_provider = entry.id.clone();
                title_model = mname.to_string();
            } else {
                tracing::warn!(model_id = %mid, "active model not found in llm settings, using current engine");
            }
        }

        let (event_tx, event_rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
        // P2 precheck: the current model is declared/remembered as text-only → images never ride the wire (cold placeholders).
        // P2 precheck keyed on the provider this execute actually activated
        // (title_provider), not the first-registered route — provider B's
        // turns must not inherit provider A's vision declaration. Memory key
        // carries the model (WI-152 S4): a text-only variant must not poison
        // a vision-capable sibling on the same provider.
        let provider_key = if title_provider.is_empty() {
            self.active_provider_key()
        } else {
            title_provider.clone()
        };
        let memory_model = if title_model.is_empty() {
            config.model.clone().unwrap_or_default()
        } else {
            title_model.clone()
        };
        let text_only_model =
            self.active_model_supports_vision_for(&provider_key, &memory_model) == Some(false);
        // WI-152: only routes with NO built-in declaration may learn a
        // text-only memory from a failed image turn — table-backed routes
        // surface the error to the user instead of silently flipping.
        let vision_memory_eligible = crate::vision::builtin_vision(&provider_key).is_none();
        let watch_rx = event_rx.resubscribe();
        // WI-97: the goal watcher's own subscription — taken before the
        // outer spawn moves event_rx.
        let goal_watch_rx = event_rx.resubscribe();
        let inner_arc = self.inner.clone();
        let handle = self.runtime_handle.clone();
        let spawn_handle = self.runtime_handle.clone();
        let title_provider = title_provider.clone();
        let title_model = title_model.clone();
        let event_tx = event_tx.clone();
        let attachment_store = Arc::clone(&self.attachment_store);
        let memory_store_for_extract = Arc::clone(&self.memory_store);
        let memory_budget_for_extract = Arc::clone(&self.memory_budget);
        let running_guard = running_guard;
        let spawn_agent_preset = agent_preset;
        let spawn_agent_name = agent_name.clone();
        // 逐字节实时一致：快照上下文在 spawn 前构建（避免借用逃逸）。
        let trace_ctx = Some(crate::plugin_kernel::TraceSnapContext {
            inner: Arc::clone(&inner_arc),
            epochs: Arc::clone(&self.trace_epochs),
        });
        handle.spawn(async move {
            let mut running_guard = running_guard;
            let kernel = {
                let l = inner_arc.lock().await;
                l.as_ref().map(|i| i.agent.clone())
            };
            let Some(kernel) = kernel else {
                return;
            };
            let cancel = tokio_util::sync::CancellationToken::new();
            let sid = session_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            // Single-input slot for FRESH sessions: the caller sent no id, so
            // the guard above was constructed empty. Adopt the minted sid so
            // `sessions.running` reports the session for the whole first turn
            // and the sidebar poll materializes the row immediately (WI-179).
            running_guard.adopt(&sid);
            // New session: apply the staged mode to the kernel first (the
            // preset is stamped on SessionCreated), and write the mode into the
            // projcache as soon as the session id exists (write-once, locked for
            // the session's life).
            if let Some(em) = fresh_mode {
                kernel.apply_execution_mode(Some(em));
                let inner = inner_arc.lock().await;
                if let Some(ref runtime) = *inner {
                    if let Err(e) = runtime.host_store.stamp_execution_mode(&sid, em.as_str()) {
                        tracing::warn!(session = %sid, error = %e, "stamp execution mode failed");
                    }
                }
            }
            // Same write-once contract for the runner agent: the projection
            // carries it so the UI can reopen a session with the agent that
            // started it (chat vs coding keep separate models/workspaces).
            {
                let inner = inner_arc.lock().await;
                if let Some(ref runtime) = *inner {
                    if let Err(e) = runtime
                        .host_store
                        .stamp_session_agent(&sid, &spawn_agent_name)
                    {
                        tracing::warn!(session = %sid, error = %e, "stamp session agent failed");
                    }
                }
            }
            let mut working_dir = working_dir;
            if isolate_worktree {
                if let Some(ref wd) = working_dir {
                    let repo = std::path::PathBuf::from(wd);
                    let st = starriver_tools::worktree::status(&repo);
                    if st.is_repo {
                        // A2a closeout: no registry — `create` reuses the
                        // session's `.worktrees/starriver-<sid8>-<slug>` dir
                        // when it already exists, and cleanup derives the
                        // worktree from git instead of a stored row.
                        match starriver_tools::worktree::create(&repo, &sid, &task, None) {
                            Ok(info) => {
                                working_dir = Some(info.path.to_string_lossy().into_owned());
                            }
                            Err(e) => eprintln!("[worktree] create failed: {e}"),
                        }
                    }
                }
            }

            // WI-7: expand @file references into budgeted fenced blocks
            // (workspace-rooted; unresolved/escaping tokens stay verbatim).
            {
                let settings = crate::settings::load_settings();
                task = crate::helpers::expand_file_references(
                    &task,
                    working_dir.as_deref().map(std::path::Path::new),
                    settings.agent.file_ref_max_bytes.unwrap_or(32 * 1024),
                    settings.agent.file_ref_total_max_bytes.unwrap_or(96 * 1024),
                );
            }

            // LLP P0: resolve markdown image references into kernel Image
            // blocks. The blocks ride the logged user message, so history/the
            // UI can render thumbnails from them.
            //
            // WI-152 S3: no OCR preprocessing — every reference either
            // resolves (rides the request per the cold/hot projection) or
            // stays verbatim in the text. Text-only models never receive
            // image data (request-time projection sheds them to placeholders).
            let (task, image_blocks) =
                crate::attachments::extract_image_blocks(&task, &attachment_store);
            // WI-152 S2 backend defense: a text-only route never receives
            // image data (composer gate blocks new attachments; this catches
            // history images after a mid-session model switch and API-direct
            // sends). The note rides the logged user message so both the
            // model and the UI see why the images are absent; the request-time
            // projection sheds the blocks to placeholders.
            //
            // extract_image_blocks now returns the position-faithful content
            // (text segments interleaved with Image blocks), so the note
            // becomes a trailing Text block instead of being glued onto the
            // (stripped) title text.
            let image_count = image_blocks
                .iter()
                .filter(|b| matches!(b, starriver_kernel::llm::ContentBlock::Image { .. }))
                .count();
            let image_blocks = if text_only_model && image_count > 0 {
                let mut blocks = image_blocks;
                blocks.push(starriver_kernel::llm::ContentBlock::Text(format!(
                    "[已忽略 {image_count} 张图片：当前模型不支持图片输入]"
                )));
                blocks
            } else {
                image_blocks
            };
            let has_images = image_count > 0;

            // Error memory: if a turn that sent images ends with an
            // "images not supported"-style error, remember the route (S4:
            // provider+model key, TTL-bounded at read time) and downgrade
            // automatically afterwards. Only undeclared routes are recorded —
            // declared/table-backed routes surface the error to the user
            // instead of silently flipping (the UI's supports_vision checkbox
            // always wins and can correct a record).
            if has_images {
                let memory_key = Self::vision_memory_key(&provider_key, &memory_model);
                let mut watch_rx = watch_rx.resubscribe();
                tokio::spawn(async move {
                    for _ in 0..600 {
                        match tokio::time::timeout(
                            std::time::Duration::from_millis(500),
                            watch_rx.recv(),
                        )
                        .await
                        {
                            Ok(Ok(ev)) if ev["type"] == "turnError" => {
                                let msg = ev["error"].as_str().unwrap_or("");
                                // GLM/bigmodel's Chinese validation error
                                // (`messages.content.type 参数非法，取值范围
                                // ['text']`) matched none of the English
                                // keywords, so the route never learned and
                                // every image turn failed identically.
                                if vision_memory_eligible
                                    && crate::vision::is_text_only_rejection(msg)
                                {
                                    Self::text_only_providers()
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .insert(memory_key, std::time::SystemTime::now());
                                }
                                break;
                            }
                            Ok(Ok(ev)) if ev["type"] == "turnEnd" => break,
                            Ok(_) => {}
                            _ => break,
                        }
                    }
                });
            }




            // Signalled when the round loop below finishes, so the goal
            // accounting watcher releases its Sender clone.
            let mut goal_done_for_rounds: Option<std::sync::Arc<tokio::sync::Notify>> = None;

            // WI-97/BUG-87: goal accounting — sum usage events into the
            // goal, flip BudgetLimited, and broadcast goalStatus. The
            // continuation itself is driven by the post-turn loop in the
            // Ok(execution) branch below: a followup injected here would be
            // swallowed by that execute's dispose() (cancel clears the
            // inbox), and the relay unsubscribes after the first terminal
            // event, so turn 2+ would never be visible anyway.
            {
                let mut goal_rx = goal_watch_rx;
                let inner_arc = inner_arc.clone();
                let sid_for_goal = sid.clone();
                let event_tx_for_goal = event_tx.clone();
                let goal_done = std::sync::Arc::new(tokio::sync::Notify::new());
                goal_done_for_rounds = Some(goal_done.clone());
                tokio::spawn(async move {
                    loop {
                        // Exit when the round loop finishes: this task must
                        // not outlive its Sender clone, or the broadcast
                        // channel (and the caller's event stream) never
                        // closes.
                        let ev = tokio::select! {
                            biased;
                            _ = goal_done.notified() => {
                                // The notify can race the relay's forwarding
                                // of the final usage events (wait_terminal
                                // watches the bus directly and may return
                                // first). Without a goal there is nothing to
                                // account — exit immediately so plain
                                // executes pay no delay; with a goal, give
                                // the relay a bounded window to land the
                                // last round's accounting.
                                let has_goal = {
                                    let inner = inner_arc.lock().await;
                                    inner
                                        .as_ref()
                                        .is_some_and(|i| i.goals.lock().unwrap().contains_key(&sid_for_goal))
                                };
                                if !has_goal {
                                    return;
                                }
                                let deadline = tokio::time::Instant::now()
                                    + std::time::Duration::from_millis(2000);
                                loop {
                                    match goal_rx.try_recv() {
                                        Ok(ev) => {
                                            account_goal_usage(
                                                &inner_arc,
                                                &event_tx_for_goal,
                                                &sid_for_goal,
                                                ev,
                                            )
                                            .await;
                                        }
                                        Err(_)
                                            if tokio::time::Instant::now() >= deadline =>
                                        {
                                            break;
                                        }
                                        Err(_) => {
                                            tokio::time::sleep(
                                                std::time::Duration::from_millis(2),
                                            )
                                            .await;
                                        }
                                    }
                                }
                                return;
                            }
                            ev = goal_rx.recv() => match ev {
                                Ok(ev) => ev,
                                Err(_) => return,
                            },
                        };
                        account_goal_usage(
                            &inner_arc,
                            &event_tx_for_goal,
                            &sid_for_goal,
                            ev,
                        )
                        .await;
                    }
                });
            }

            // (BUG-7 contract) The working dir rides the execute call's `cwd`
            // below — a process-wide set_current_dir here raced every other
            // session's relative-path resolution and was never restored.

            let mut round_task = task.clone();
            let mut goal_rounds = 0usize;
            // File the session under its workspace BEFORE the first execution
            // spawns (directory at creation). The old post-spawn attach raced
            // TaskStarted: the UI's immediate sidebar refresh read the
            // projection before the projcache row existed, so a fresh task
            // only showed up under its workspace at turn end.
            if let Some(wd) = working_dir.as_deref() {
                let inner = inner_arc.lock().await;
                if let Some(ref runtime) = *inner {
                    // WI-164: a failed filing used to vanish — the
                    // session would silently show up ungrouped.
                    if let Err(error) = runtime.host_store.attach_by_path(wd, &sid) {
                        tracing::warn!(workspace = %wd, session = %sid, error = %error, "session workspace filing failed; session stays ungrouped");
                    }
                }
            }
            loop {
                let execution = match kernel
                    .execute_with_meta_in(
                        round_task.clone(),
                        Some(sid.clone()),
                        model_override.clone(),
                        event_tx.clone(),
                        cancel.clone(),
                        Some(crate::plugin_kernel::HostSessionMeta {
                            title: Some(crate::helpers::truncate_title(&round_task, 40)),
                            // Text-only model: images never ride the wire —
                            // request-time projection goes all-cold
                            // (metadata placeholders replace Image blocks).
                            image_hot_override: if text_only_model {
                                Some((0, 0))
                            } else {
                                None
                            },
                        }),
                        // Stamp the resolved workspace root (or the worktree when
                        // isolated) into the header — the sandbox boundary.
                        working_dir.clone(),
                        image_blocks.clone(),
                        spawn_agent_preset.clone(),
                        permission_bundle.clone(),
                        trace_ctx.clone(),
                    )
                    .await
                {
                    Ok(execution) => execution,
                    Err(e) => {
                        // TaskFailed contract requires session_id (matching the loop tx_err path)
                        let _ = event_tx.send(serde_json::json!({
                            "type": "TaskFailed",
                            "session_id": sid.clone(),
                            "error": e.to_string(),
                        }));
                        break;
                    }
                };
                {
                    // WI-103: mid-run route rewrites — a later
                    // set_agent_active_model for this agent lands on every
                    // subsequent LLM call of this turn.
                    let _route_rewrite = kernel
                        .register_route_rewrite(execution.agent(), agent_name.clone());
                    // Wait for the turn to settle so the agent handle stays
                    // alive until its events have drained, then dispose.
                    let agent = execution.agent().clone();
                    let bus = agent.ctx().event_bus().cloned().expect("agent bus present");
                    // BUG (Send Now, 2026-10-03 session 7059ac24): a flushed
                    // queue message is claimed by the agent's own driver as
                    // the next turn the instant the aborted one settles —
                    // disposing after the FIRST terminal killed that follow-up
                    // as `aborted: disposed`. Wait for every consecutive
                    // driver turn: stop only when the agent is idle (double-
                    // checked after a yield, matching when_idle) AND no
                    // next-turn message is pending. A PARKED queue is the
                    // exception: a manual stop holds it on purpose (the
                    // driver must not auto-continue), so with no live turn
                    // there is no follow-up to wait for — waiting here would
                    // hang the round loop forever (wait_terminal sees no
                    // terminal event) and leak the session input slot; the
                    // parked messages stay durable and resume with the next
                    // execute.
                    loop {
                        crate::plugin_kernel::wait_terminal(
                            Arc::clone(&agent),
                            bus.clone(),
                        )
                        .await;
                        use starriver_kernel::agent::AgentStatus;
                        if agent.status() == AgentStatus::Idle {
                            tokio::task::yield_now().await;
                            if agent.status() == AgentStatus::Idle
                                && (agent.pending_messages().next_turn.is_empty()
                                    || agent.is_parked())
                            {
                                break;
                            }
                        }
                    }

                    // Cross-agent memory (docs/36 path B): extract durable
                    // facts from the settled turn in the background. Every
                    // failure inside stays silent — memory is a bonus, never a
                    // turn dependency.
                    if agent_name == crate::CODING_AGENT_NAME {
                        crate::memory_pipeline::spawn_session_extraction(
                            inner_arc.clone(),
                            spawn_handle.clone(),
                            sid.clone(),
                            project_id.clone(),
                            memory_store_for_extract.clone(),
                            memory_budget_for_extract.clone(),
                            memory_cfg.clone(),
                            title_provider.clone(),
                            title_model.clone(),
                        );
                    }
                    execution.dispose();

                    // Upgrade the fallback title to a semantic one in the
                    // background; every failure path inside stays warn-only
                    // and the user pin always wins.
                    crate::title::spawn_detached(
                        inner_arc.clone(),
                        spawn_handle.clone(),
                        sid.clone(),
                        title_provider.clone(),
                        title_model.clone(),
                    );
                }
                // WI-97/BUG-87: goal continuation — drive the next round as a
                // fresh execute (fresh relay, fresh route rewrite, events and
                // the title/memory pipelines all reused per round). A
                // BudgetLimited goal already flipped off Active above and
                // stops here.
                let cont = {
                    let inner = inner_arc.lock().await;
                    inner
                        .as_ref()
                        .and_then(|i| crate::agents::goal_continuation_for(i, &sid))
                };
                match cont {
                    Some(c) => {
                        // Safety valve: a provider that never reports usage
                        // can never flip BudgetLimited — cap the autonomous
                        // rounds so one execute cannot loop forever.
                        if goal_rounds >= 25 {
                            tracing::warn!(session = %sid, rounds = goal_rounds,
                                "goal round cap reached (usage not reported?); stopping the loop");
                            break;
                        }
                        goal_rounds += 1;
                        round_task = c;
                    }
                    None => break,
                }
            }
            // Release the goal-accounting watcher so the event channel can
            // close (it holds a Sender clone).
            if let Some(done) = goal_done_for_rounds.take() {
                done.notify_one();
            }

            if isolate_worktree {
                let inner = inner_arc.lock().await;
                // Cleanup only while the runtime is still alive (the lock
                // doubles as the liveness guard; the git ops need nothing
                // else from it).
                if inner.is_some() {
                    // A2a closeout: derive the session worktree from git — the
                    // registry row lived in the legacy store being retired.
                    // Worktrees are named starriver-<sid8>-<slug> under the
                    // repo's .worktrees/, so the owning checkout is found via
                    // the session working dir and `git worktree list`.
                    let session_worktree = working_dir
                        .clone()
                        .map(std::path::PathBuf::from)
                        .and_then(|wd| starriver_tools::worktree::main_repo_of(&wd).ok())
                        .and_then(|repo| {
                            let short_id: String = sid.chars().take(8).collect();
                            starriver_tools::worktree::list(&repo)
                                .ok()
                                .and_then(|entries| {
                                    entries.into_iter().find(|info| {
                                        !info.is_main
                                            && info
                                                .path
                                                .file_name()
                                                .and_then(|n| n.to_str())
                                                .is_some_and(|n| {
                                                    n.starts_with(&format!("starriver-{short_id}-"))
                                                })
                                    })
                                })
                        });
                    if let Some(info) = session_worktree {
                        let wt = info.path;
                        let st = starriver_tools::worktree::status(&wt);
                        let base = starriver_tools::worktree::main_repo_of(&wt)
                            .ok()
                            .map(|repo| starriver_tools::worktree::status(&repo).branch)
                            .flatten();
                        let ahead = base
                            .as_deref()
                            .map(|b| starriver_tools::worktree::count_ahead(&wt, b).unwrap_or(1))
                            .unwrap_or(1);
                        // "kept" is implicit now: a worktree that survives one
                        // cleanup is simply re-evaluated on the next run.
                        if starriver_tools::worktree::should_remove(&st, ahead) {
                            if let Err(e) = starriver_tools::worktree::remove(&wt, base.as_deref())
                            {
                                eprintln!("[worktree] cleanup remove failed: {e}");
                            }
                        }
                    }
                }
            }
        });
        Ok(event_rx)
    }


    pub async fn shutdown(&self) -> anyhow::Result<()> {
        // Only do the final checkpoint; do not drop the runtime here:
        // the shutdown future runs on this runtime's own worker, and dropping the runtime itself would panic
        // (tokio: cannot drop a runtime from within its own async context).
        // The runtime lives as long as HostRuntime itself.
        let l = self.inner.lock().await;
        if let Some(inner) = l.as_ref() {
            // WI-164: the final checkpoint flush is the last chance to
            // persist the tail of every live session — dropping its error
            // silently meant quiet data loss at shutdown.
            if let Err(error) = inner.persistence.flush().await {
                tracing::warn!(error = %error, "final session persistence flush failed at shutdown");
            }
        }
        Ok(())
    }
}

/// Accept only the wire formats the product can actually route; anything
/// else fails loudly instead of silently falling back to chat completions.
fn validate_api_format(api_format: &str) -> Result<(), String> {
    if matches!(api_format, "openai-chat" | "openai-responses" | "anthropic") {
        Ok(())
    } else {
        Err(format!(
            "unsupported api_format `{api_format}` (expected openai-chat, openai-responses, or anthropic)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-04 regression: initial MCP discovery must stay off the
    /// `HostRuntime::new` critical path. Inline discovery made the Electron
    /// window (created only after backend init) wait out an unreachable
    /// server's ~75s TCP connect timeout — the app sat in the dock looking
    /// dead (host.log 2026-10-04, server "nexplan" at a sleeping LAN host).
    /// 192.0.2.1 is TEST-NET-1: the connect can never succeed, only time out.
    #[test]
    fn host_runtime_new_is_not_blocked_by_an_unreachable_mcp_server() {
        crate::testutil::with_temp_home(|home| {
            std::fs::create_dir_all(home.join(".starriver")).unwrap();
            std::fs::write(
                home.join(".starriver").join("mcp.yaml"),
                "mcp_servers:\n  blackhole:\n    transport: http\n    command: ''\n    url: http://192.0.2.1:9/mcp\n",
            )
            .unwrap();
            let started = std::time::Instant::now();
            let rt = HostRuntime::new(None).expect("runtime constructs");
            let elapsed = started.elapsed();
            drop(rt);
            assert!(
                elapsed < std::time::Duration::from_secs(10),
                "HostRuntime::new took {}s with an unreachable MCP server configured; \
                 discovery must not block startup",
                elapsed.as_secs()
            );
        });
    }

    /// WI-164: a failed tool registration must warn, not vanish — a second
    /// registration of the same name fails deterministically
    /// (`AlreadyRegistered`).
    #[test]
    fn register_tool_or_warn_surfaces_rejection() {
        struct Echo;
        impl starriver_kernel::tools::ToolExecutor for Echo {
            fn execute(
                &self,
                _arguments: cordis_rslite::Value,
                _ctx: starriver_kernel::tools::ToolRunContext,
            ) -> cordis_rslite::BoxFuture<
                Result<cordis_rslite::Value, starriver_kernel::tools::ToolError>,
            > {
                Box::pin(async { Ok(cordis_rslite::Value::new(())) })
            }
        }
        let tools = starriver_tools::InMemoryToolRuntime::new(
            cordis_rslite::EventBus::new(),
            cordis_rslite::Context::new(),
        );
        let definition = starriver_kernel::tools::ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: "probe_tool".into(),
            description: "probe".into(),
            parameters: cordis_rslite::Value::new(()),
            output: cordis_rslite::Value::new(()),
            render: std::sync::Arc::new(|_, value| {
                vec![starriver_kernel::llm::ContentBlock::Text(format!("{value:?}"))]
            }),
            executor: std::sync::Arc::new(Echo),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        };
        register_tool_or_warn(&tools, definition.clone(), "probe_tool");
        let log = crate::logging::testutil::capture_tracing(|| {
            register_tool_or_warn(&tools, definition, "probe_tool");
        });
        assert!(
            log.contains("host tool registration failed"),
            "expected the registration warn, got: {log}"
        );
        assert!(
            log.contains("probe_tool") && log.contains("already registered"),
            "warn should name the tool and the cause: {log}"
        );
    }

    #[test]
    fn api_format_validation_accepts_known_formats() {
        for format in ["openai-chat", "openai-responses", "anthropic"] {
            assert!(validate_api_format(format).is_ok(), "{format}");
        }
    }

    #[test]
    fn api_format_validation_rejects_unknown_formats() {
        let err = validate_api_format("gpt-4-vision").expect_err("must reject");
        assert!(err.contains("unsupported api_format"), "{err}");
    }

    #[test]
    fn credential_ref_from_distributes_reference_forms() {
        // `!file:` → secrets account reference (never resolved early)
        match HostRuntime::credential_ref_from("!file:model/ds") {
            Some(OpenAiCredentialRef::File(account)) => assert_eq!(account, "model/ds"),
            other => panic!("expected File ref, got {other:?}"),
        }
        // `${ENV}` → env reference
        match HostRuntime::credential_ref_from("${DEEPSEEK_API_KEY}") {
            Some(OpenAiCredentialRef::Env(var)) => assert_eq!(var, "DEEPSEEK_API_KEY"),
            other => panic!("expected Env ref, got {other:?}"),
        }
        // literal keys stay literal (legacy configs)
        match HostRuntime::credential_ref_from("sk-plain") {
            Some(OpenAiCredentialRef::Literal(key)) => assert_eq!(key, "sk-plain"),
            other => panic!("expected Literal ref, got {other:?}"),
        }
        // empty fields carry no credential at all
        assert!(HostRuntime::credential_ref_from("").is_none());
    }

    /// WI-152 S4: the TTL check is pure — fresh entries count, aged entries
    /// expire, and `ttl_hours == 0` means "keep forever".
    #[test]
    fn vision_error_memory_ttl_is_pure_and_zero_means_forever() {
        let now = std::time::SystemTime::now();
        assert!(HostRuntime::memory_entry_current(now, 168, now));
        let old = now - std::time::Duration::from_secs(169 * 3600);
        assert!(!HostRuntime::memory_entry_current(old, 168, now));
        assert!(
            HostRuntime::memory_entry_current(old, 0, now),
            "0 = forever"
        );
    }

    /// WI-152 S4: the error memory is keyed per provider+model and only
    /// consulted for routes WITHOUT a built-in declaration — a table-backed
    /// provider is never flipped by a runtime error record.
    #[test]
    fn vision_error_memory_is_route_scoped_and_table_shielded() {
        crate::testutil::with_temp_home(|_home| {
            let host = crate::HostRuntime::new(None).expect("host boots");
            host.add_model(
                "relay".into(),
                "Relay".into(),
                "cloud".into(),
                "openai-compat".into(),
                Some("https://relay.example.com".into()),
                Some("sk-test".into()),
                None,
                "relay-chat".into(),
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
            let record = |provider: &str, model: &str| {
                HostRuntime::text_only_providers().lock().unwrap().insert(
                    HostRuntime::vision_memory_key(provider, model),
                    std::time::SystemTime::now(),
                );
            };

            // Route granularity: relay/m1 poisoned → relay/m2 unaffected.
            record("relay", "m1");
            assert_eq!(
                host.active_model_supports_vision_for("relay", "m1"),
                Some(false)
            );
            assert_eq!(host.active_model_supports_vision_for("relay", "m2"), None);

            // Table-backed providers are never flipped by a memory record.
            host.add_model(
                "deepseek".into(),
                "DeepSeek".into(),
                "cloud".into(),
                "openai-compat".into(),
                Some("https://api.deepseek.com".into()),
                Some("sk-test".into()),
                None,
                "deepseek-chat".into(),
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
            .expect("add deepseek");
            record("deepseek", "deepseek-chat");
            assert_eq!(
                host.active_model_supports_vision_for("deepseek", "deepseek-chat"),
                Some(true),
                "builtin-table providers ignore error memory"
            );
        });
    }

    #[test]
    fn resolve_credential_field_resolves_refs_and_normalizes_missing() {
        // Env var refs resolve through the environment.
        let unique = format!("STARRIVER_CRED_TEST_{}", uuid::Uuid::new_v4().simple());
        std::env::set_var(&unique, "sk-env");
        assert_eq!(
            HostRuntime::resolve_credential_field(&format!("${{{unique}}}")).unwrap(),
            "sk-env"
        );
        std::env::remove_var(&unique);

        // Missing secrets account → MISSING_CREDENTIAL (not a transport error).
        let err = HostRuntime::resolve_credential_field("!file:model/does-not-exist").unwrap_err();
        assert_eq!(err.code(), "MISSING_CREDENTIAL");

        // Empty field → MISSING_CREDENTIAL.
        let err = HostRuntime::resolve_credential_field("").unwrap_err();
        assert_eq!(err.code(), "MISSING_CREDENTIAL");
    }
}

/// WI-97/BUG-87: fold one `usage` event into the session's goal, flip
/// BudgetLimited, and broadcast the goalStatus projection.
async fn account_goal_usage(
    inner_arc: &std::sync::Arc<tokio::sync::Mutex<Option<crate::RuntimeInner>>>,
    event_tx: &tokio::sync::broadcast::Sender<serde_json::Value>,
    sid: &str,
    ev: serde_json::Value,
) {
    if ev["type"].as_str() != Some("usage") {
        return;
    }
    let tokens = ev["inputTokens"].as_u64().unwrap_or(0)
        .saturating_add(ev["outputTokens"].as_u64().unwrap_or(0));
    let broadcast = {
        let inner = inner_arc.lock().await;
        let Some(inner) = inner.as_ref() else { return };
        let mut goals = inner.goals.lock().unwrap();
        let Some(g) = goals.get_mut(sid) else { return };
        g.tokens_used = g.tokens_used.saturating_add(tokens);
        if let Some(budget) = g.token_budget {
            if g.tokens_used >= budget && g.status == crate::GoalStatus::Active {
                g.status = crate::GoalStatus::BudgetLimited;
            }
        }
        let mut payload = serde_json::json!({
            "type": "goalStatus",
            "version": 1,
            "sessionId": sid,
            "objective": g.objective,
            "status": g.status.as_str(),
            "tokensUsed": g.tokens_used,
        });
        if let Some(budget) = g.token_budget {
            payload["tokenBudget"] = serde_json::json!(budget);
        }
        payload
    };
    let _ = event_tx.send(broadcast);
}

// ── WI-163: MCP discovery retry wiring ──────────────────────────────────────
// A failed discovery used to be a permanent silent skip (warn dropped, no
// subscriber; no retry; sessions never learned their tools were missing).
// The helpers below give every failure path a self-healing continuation:
// discovery fail → background retry → success → register + supervise;
// supervision gives up → mark unavailable → background retry again.

/// WI-164: register a host tool, warning instead of silently dropping — a
/// swallowed `AlreadyRegistered`/kernel rejection meant the tool just never
/// existed for sessions.
fn register_tool_or_warn(
    tools: &starriver_tools::InMemoryToolRuntime,
    definition: starriver_kernel::tools::ToolDefinition,
    tool: &str,
) {
    if let Err(error) = tools.register(definition) {
        tracing::warn!(tool, error = %error, "host tool registration failed; tool will be missing");
    }
}

/// Re-register a server's definitions after (re-)discovery and refresh the
/// derived state: the UI tool list and the availability notice feed.
async fn reregister_discovered(
    agent: Arc<crate::plugin_kernel::PluginKernel>,
    mcp_tools: Arc<RwLock<std::collections::HashMap<String, Vec<String>>>>,
    mcp_unavailable: Arc<crate::mcp_status_state::McpUnavailableServers>,
    name: String,
    definitions: Vec<starriver_kernel::tools::ToolDefinition>,
) {
    if let Err(error) = agent.register_mcp_definitions(&name, definitions.clone()).await {
        tracing::warn!(server = %name, error = %error, "kernel MCP registration failed");
    }
    mcp_tools.write().unwrap().insert(
        name.clone(),
        definitions.iter().map(|d| d.name.clone()).collect(),
    );
    mcp_unavailable.clear(&name);
}

/// WI-20 + WI-163: supervise a live discovered connection. On restart the
/// definitions are re-registered (the tool list may have changed); on
/// give-up the server is marked unavailable and long-horizon discovery
/// retry takes over.
#[allow(clippy::too_many_arguments)]
fn supervise_mcp(
    agent: Arc<crate::plugin_kernel::PluginKernel>,
    mcp_tools: Arc<RwLock<std::collections::HashMap<String, Vec<String>>>>,
    mcp_unavailable: Arc<crate::mcp_status_state::McpUnavailableServers>,
    bus: cordis_rslite::EventBus,
    handle: tokio::runtime::Handle,
    name: String,
    srv: crate::mcp::config::McpServerConfig,
    token_provider: crate::mcp::adapter::TokenProvider,
    connection: Arc<tokio::sync::Mutex<crate::mcp::transport::Connection>>,
) {
    let recovery_agent = Arc::clone(&agent);
    let recovery_tools = Arc::clone(&mcp_tools);
    let recovery_unavailable = Arc::clone(&mcp_unavailable);
    let recovery_server = name.clone();
    let dead_agent = Arc::clone(&agent);
    let dead_tools = Arc::clone(&mcp_tools);
    let dead_unavailable = Arc::clone(&mcp_unavailable);
    let dead_bus = bus.clone();
    let dead_handle = handle.clone();
    let dead_server = name.clone();
    let dead_srv = srv.clone();
    let dead_token = token_provider.clone();
    let monitor = crate::mcp::health::McpHealthMonitor::new(name, srv, token_provider, connection)
        .with_event_bus(bus)
        .on_recovery(Arc::new(move |definitions| {
            let agent = Arc::clone(&recovery_agent);
            let tools = Arc::clone(&recovery_tools);
            let unavailable = Arc::clone(&recovery_unavailable);
            let server = recovery_server.clone();
            Box::pin(async move {
                reregister_discovered(agent, tools, unavailable, server, definitions).await;
            })
        }))
        .on_dead(Arc::new(move |detail| {
            let agent = Arc::clone(&dead_agent);
            let tools = Arc::clone(&dead_tools);
            let unavailable = Arc::clone(&dead_unavailable);
            let bus = dead_bus.clone();
            let handle = dead_handle.clone();
            let name = dead_server.clone();
            let srv = dead_srv.clone();
            let token_provider = dead_token.clone();
            Box::pin(async move {
                tracing::warn!(server = %name, detail = %detail, "MCP supervision gave up; falling back to discovery retry");
                unavailable.mark(&name, detail);
                spawn_discovery_retry(agent, tools, unavailable, bus, handle, name, srv, token_provider);
            })
        }));
    // Detached: supervision runs for the process lifetime and dies with the
    // runtime.
    monitor.start_on(&handle);
}

/// WI-163: keep retrying a failed discovery in the background (10s → 60s
/// backoff, forever). Success re-registers the tools, clears the
/// unavailability notice, and starts a fresh health monitor.
#[allow(clippy::too_many_arguments)]
fn spawn_discovery_retry(
    agent: Arc<crate::plugin_kernel::PluginKernel>,
    mcp_tools: Arc<RwLock<std::collections::HashMap<String, Vec<String>>>>,
    mcp_unavailable: Arc<crate::mcp_status_state::McpUnavailableServers>,
    bus: cordis_rslite::EventBus,
    handle: tokio::runtime::Handle,
    name: String,
    srv: crate::mcp::config::McpServerConfig,
    token_provider: crate::mcp::adapter::TokenProvider,
) {
    let success_agent = Arc::clone(&agent);
    let success_tools = Arc::clone(&mcp_tools);
    let success_unavailable = Arc::clone(&mcp_unavailable);
    let success_bus = bus.clone();
    let success_handle = handle.clone();
    let success_server = name.clone();
    let success_srv = srv.clone();
    let success_token = token_provider.clone();
    crate::mcp::health::McpDiscoveryRetry::new(name, srv, token_provider)
        .with_policy(crate::mcp::health::RestartPolicy {
            initial_backoff: Duration::from_secs(10),
            max_backoff: Duration::from_secs(60),
            ..Default::default()
        })
        .with_event_bus(bus)
        .on_success(Arc::new(move |definitions, connection| {
            let agent = Arc::clone(&success_agent);
            let tools = Arc::clone(&success_tools);
            let unavailable = Arc::clone(&success_unavailable);
            let bus = success_bus.clone();
            let handle = success_handle.clone();
            let name = success_server.clone();
            let srv = success_srv.clone();
            let token_provider = success_token.clone();
            Box::pin(async move {
                reregister_discovered(
                    Arc::clone(&agent),
                    Arc::clone(&tools),
                    Arc::clone(&unavailable),
                    name.clone(),
                    definitions,
                )
                .await;
                supervise_mcp(
                    agent,
                    tools,
                    unavailable,
                    bus,
                    handle,
                    name,
                    srv,
                    token_provider,
                    connection,
                );
            })
        }))
        .start_on(&handle);
}
