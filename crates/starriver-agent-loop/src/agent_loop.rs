//! AgentLoop factory: composes session + agent + registry publication and
//! drives configured agents, matching `the agent loop` service.

use std::sync::Arc;

use cordis_rslite::{BoxFuture, Context, Value};
use starriver_kernel::agent::{
    Agent, AgentError, AgentFactory, AgentHandle, AgentOptions, AgentSessionStart,
    CreateAgentOptions, ResumeAgentOptions, SessionStartSource,
};
use starriver_kernel::session::{PrepareOptions, Session, SessionId};

use starriver_kernel::system_prompt::SystemPromptRegistry;

use crate::agent::ReactLoopAgent;
use crate::approval::{ApprovalPolicy, ApprovalService};
use crate::services::{LlmRuntimeView, PersistenceView, SessionStoreView, ToolRuntimeView};
use starriver_kernel::agent::AgentRegistry;

/// One agent's stop capability: cancel + quiesce + unwind its scope.
pub type AgentStop = Arc<dyn Fn() + Send + Sync>;
/// Live-agent stop closures owned by the factory.
pub type LiveAgents = Arc<std::sync::Mutex<Vec<AgentStop>>>;

#[derive(Debug, Clone, Default)]
pub struct AgentLoopConfig {
    /// Maximum in-flight parallel-safe tool calls per step (1 = serial).
    pub max_parallel_tool_calls: usize,
    /// Configured agents started via [`AgentLoop::start_configured`].
    pub agents: Vec<AgentConfig>,
}

#[derive(Debug, Clone, Default)]
pub struct AgentConfig {
    pub id: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub max_tokens: Option<u64>,
    pub cwd: Option<String>,
 /// Durable session to resume instead of creating fresh (resumeSessionId).
    pub resume_session_id: Option<String>,
}

/// Render the `sandbox:policy` runtime-context contribution: the effective
/// file-effect mode folded from the session log plus the writable root from
/// the frozen header. No mode event → empty (the registry join drops it).
fn sandbox_policy_text(session: &Session) -> String {
    match starriver_kernel::sandbox::effective_sandbox_mode(session) {
        Some(mode) => {
            let root = session.header.cwd.as_deref().unwrap_or("not set");
            match mode {
                starriver_kernel::sandbox::SandboxMode::ReadOnly => {
                    "Sandbox policy: read-only. The sandbox refuses file-writing tools.".to_string()
                }
                starriver_kernel::sandbox::SandboxMode::WorkspaceWrite => {
                    format!("Sandbox policy: workspace-write. File writes are confined to {root}.")
                }
                starriver_kernel::sandbox::SandboxMode::DangerFullAccess => {
                    "Sandbox policy: danger-full-access. Execution is not constrained.".to_string()
                }
            }
        }
        None => String::new(),
    }
}

/// Resolve a preset's default agent options, then apply caller overrides
/// field-by-field. A missing preset or absent preset id keeps the caller's
/// options unchanged. Matches: preset decides the composition, options
/// override it.
pub fn resolve_preset_options(
    preset_registry: &std::collections::HashMap<String, starriver_kernel::agent::AgentOptions>,
    preset: Option<&str>,
    caller: starriver_kernel::agent::AgentOptions,
) -> starriver_kernel::agent::AgentOptions {
    let Some(preset) = preset.and_then(|id| preset_registry.get(id)) else {
        return caller;
    };
    starriver_kernel::agent::AgentOptions {
        compaction_tuning: caller
            .compaction_tuning
            .or_else(|| preset.compaction_tuning.clone()),
        provider: caller.provider.or_else(|| preset.provider.clone()),
        model: caller.model.or_else(|| preset.model.clone()),
        max_tokens: caller.max_tokens.or(preset.max_tokens),
        system: caller.system.or_else(|| preset.system.clone()),
        system_sections: if caller.system_sections.is_empty() {
            preset.system_sections.clone()
        } else {
            caller.system_sections
        },
        visible_tools: caller
            .visible_tools
            .or_else(|| preset.visible_tools.clone()),
        time_refresh_interval_ms: caller
            .time_refresh_interval_ms
            .or(preset.time_refresh_interval_ms),
        prompt_settings: caller
            .prompt_settings
            .or_else(|| preset.prompt_settings.clone()),
        context_window: caller.context_window.or(preset.context_window),
        compaction_threshold_ratio: caller
            .compaction_threshold_ratio
            .or(preset.compaction_threshold_ratio),
        compaction_retain_ratio: caller
            .compaction_retain_ratio
            .or(preset.compaction_retain_ratio),
        image_projection: caller.image_projection.or(preset.image_projection),
        image_resolver: caller
            .image_resolver
            .or_else(|| preset.image_resolver.clone()),
        // WI-163: the availability feed is runtime state, not a preset
        // property — the caller (host per-run options) always wins.
        mcp_status_feed: caller
            .mcp_status_feed
            .or_else(|| preset.mcp_status_feed.clone()),
        max_parallel_tool_calls: caller
            .max_parallel_tool_calls
            .or(preset.max_parallel_tool_calls),
        turn_tool_call_budget: caller
            .turn_tool_call_budget
            .or(preset.turn_tool_call_budget),
    }
}

/// Concrete factory that builds published `ReactLoopAgent`s. Services are
/// injected as object-safe view traits at the composition root.
pub struct AgentLoop {
    ctx: Context,
    config: AgentLoopConfig,
    sessions: Arc<dyn SessionStoreView>,
    tools: Arc<dyn ToolRuntimeView>,
    llm: Arc<dyn LlmRuntimeView>,
    registry: Arc<dyn AgentRegistry>,
    persistence: Option<Arc<dyn PersistenceView>>,
    /// Shared preset registry: the host upserts per-agent presets (built
    /// from AGENT.md `preset:` blocks) into the same map the factory reads
    /// at create/resume, so a header-locked preset resolves after restart.
    preset_registry: Arc<std::sync::Mutex<std::collections::HashMap<String, starriver_kernel::agent::AgentOptions>>>,
    scopes: Arc<starriver_kernel::agent::AgentScopeRegistry>,
    system_prompts: Arc<SystemPromptRegistry>,
    /// Live-agent stop closures, run on factory teardown: cancel + quiesce +
    /// unwind the agent scope. Full registry/session detachment stays with the
    /// consumer's handle.
    live: LiveAgents,
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ctx: Context,
        config: AgentLoopConfig,
        sessions: Arc<dyn SessionStoreView>,
        tools: Arc<dyn ToolRuntimeView>,
        llm: Arc<dyn LlmRuntimeView>,
        registry: Arc<dyn AgentRegistry>,
        persistence: Option<Arc<dyn PersistenceView>>,
        preset_registry: std::collections::HashMap<String, starriver_kernel::agent::AgentOptions>,
        scopes: Arc<starriver_kernel::agent::AgentScopeRegistry>,
        system_prompts: Arc<SystemPromptRegistry>,
    ) -> Self {
        Self::new_shared(
            ctx,
            config,
            sessions,
            tools,
            llm,
            registry,
            persistence,
            Arc::new(std::sync::Mutex::new(preset_registry)),
            scopes,
            system_prompts,
        )
    }

    /// Like [`Self::new`], but takes an already-shared preset registry so the
    /// host can upsert entries after construction.
    #[allow(clippy::too_many_arguments)]
    pub fn new_shared(
        ctx: Context,
        config: AgentLoopConfig,
        sessions: Arc<dyn SessionStoreView>,
        tools: Arc<dyn ToolRuntimeView>,
        llm: Arc<dyn LlmRuntimeView>,
        registry: Arc<dyn AgentRegistry>,
        persistence: Option<Arc<dyn PersistenceView>>,
        preset_registry: Arc<std::sync::Mutex<std::collections::HashMap<String, starriver_kernel::agent::AgentOptions>>>,
        scopes: Arc<starriver_kernel::agent::AgentScopeRegistry>,
        system_prompts: Arc<SystemPromptRegistry>,
    ) -> Self {
        // Approval seam: register the service and its runtime-context policy
        // projection. The tools layer reads it as `approval` and degrades to
        // deny when a deployment composes no service.
        let bus = ctx.event_bus().expect("agent loop requires an event bus");
        let approval = ApprovalService::new(
            bus.clone(),
            Arc::clone(&registry) as Arc<dyn AgentRegistry>,
            ApprovalPolicy::Ask,
        );
        let _approval_handle = ctx.provide_any(
            "approval",
            Value::from_arc(Arc::new(starriver_kernel::approval::ApprovalHandle(
                approval as Arc<dyn starriver_kernel::approval::ApprovalRuntime>,
            ))),
        );
        // SP1b/SP2: the runtime-context contributions render from the shared
        // SystemPromptRegistry — sandbox:policy (order 110) first-class beside
        // the approval projection.
        let _sandbox_context = system_prompts
            .context(
                &ctx,
                starriver_kernel::context::PromptContext {
                    name: "sandbox:policy".into(),
                    order: 110,
                    render: Arc::new(|render| sandbox_policy_text(render.session)),
                },
            )
            .ok();
        let _policy_context = system_prompts
            .context(
                &ctx,
                starriver_kernel::context::PromptContext {
                    name: "approval:policy".into(),
                    order: 115,
                    render: Arc::new(|render| {
                        match crate::approval::effective_policy(render.session) {
                            Some(ApprovalPolicy::Never) => {
                                "Approval prompts are disabled in this session: actions that require approval are rejected automatically — do not request escalation.".to_string()
                            }
                            Some(ApprovalPolicy::Ask) => {
                                "Approval policy: ask. Operations that require approval may ask the user; without an available answerer, the request fails closed.".to_string()
                            }
                            // No policy event: the session never opted into approval
                            // policy awareness, so keep the model transcript clean.
                            None => String::new(),
                        }
                    }),
                },
            )
            .ok();
        // PTC prompt half of the ptc-only contract. While the mode is inactive
        // both render empty; when active, the toolchain section declares the
        // program dialect and the callable catalog (schemas WITHOUT run_code),
        // and the ptc-only rule states the same predicate the run_code guard
        // enforces — prompt and registry cannot drift apart.
        let ptc_sdk_tools = Arc::clone(&tools);
        let _ptc_sdk_context = system_prompts
            .context(
                &ctx,
                starriver_kernel::context::PromptContext {
                    name: "tools:ptc-sdk".into(),
                    order: 120,
                    render: Arc::new(move |_| {
                        if ptc_sdk_tools.is_ptc_active() {
                            starriver_kernel::ptc::sdk_section_text(&ptc_sdk_tools.sdk_schemas())
                        } else {
                            String::new()
                        }
                    }),
                },
            )
            .ok();
        let ptc_only_tools = Arc::clone(&tools);
        let _ptc_only_context = system_prompts
            .context(
                &ctx,
                starriver_kernel::context::PromptContext {
                    name: "tools:ptc-only".into(),
                    order: 125,
                    render: Arc::new(move |_| {
                        if ptc_only_tools.is_ptc_active() {
                            starriver_kernel::ptc::PTC_ONLY_TEXT.to_string()
                        } else {
                            String::new()
                        }
                    }),
                },
            )
            .ok();
        let service = Self {
            ctx,
            config,
            sessions,
            tools,
            llm,
            registry,
            persistence,
            preset_registry,
            scopes,
            system_prompts,
            live: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        // Factory-level ownership: disposing the loop context stops every
        // live agent.
        let live = Arc::clone(&service.live);
        let _ = service.ctx.effect(move || {
            let stops = std::mem::take(&mut *live.lock().unwrap());
            for stop in stops {
                stop();
            }
        });
        service
    }

    /// Stop every live agent and unwind its scope (factory teardown).
    /// Shared handle onto the preset registry: the host upserts per-agent
    /// presets into the same map the factory resolves at create/resume.
    pub fn preset_registry_handle(
        &self,
    ) -> Arc<std::sync::Mutex<std::collections::HashMap<String, starriver_kernel::agent::AgentOptions>>>
    {
        Arc::clone(&self.preset_registry)
    }

    fn stop_all(&self) {
        let stops = std::mem::take(&mut *self.live.lock().unwrap());
        for stop in stops {
            stop();
        }
    }

    /// Publish one agent from a prepared session: enter both registries,
    /// announce with veto rollback, notify session-start, then start the
    /// driver. Returns a consumer-owned handle.
 /// Create-path preset resolution (binding parity): a fresh session
    /// naming a preset composes from it immediately, symmetric with the
    /// resume path's header-driven lookup. Unknown ids resolve to the caller
    /// options — the host validates existence/brokenness before the run.
    fn resolve_create_options(&self, options: CreateAgentOptions) -> CreateAgentOptions {
        if options.agent_preset.is_none() {
            return options;
        }
        let registry = self.preset_registry.lock().unwrap_or_else(|e| e.into_inner());
        let agent_options = resolve_preset_options(&registry, options.agent_preset.as_deref(), options.agent_options);
        CreateAgentOptions {
            agent_options,
            ..options
        }
    }

    async fn publish(&self, options: CreateAgentOptions) -> Result<AgentHandle, AgentError> {
        let options = self.resolve_create_options(options);
        publish_agent(
            self.ctx.clone(),
            Arc::clone(&self.sessions),
            Arc::clone(&self.tools),
            Arc::clone(&self.llm),
            Arc::clone(&self.registry),
            Arc::clone(&self.live),
            Arc::clone(&self.scopes),
            Arc::clone(&self.system_prompts),
            options,
        )
        .await
    }

    /// Start every configured agent and return their handles. Agents with a
    /// `resume_session_id` resume that durable session (NotFound falls back to
    /// create); the rest create fresh sessions.
    pub async fn start_configured(&self) -> Vec<AgentHandle> {
        let mut handles = Vec::new();
        for config in &self.config.agents {
            let agent_options = starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: config.provider.clone(),
                model: config.model.clone(),
                max_tokens: config.max_tokens,
                system: None,
                ..Default::default()
            };
            let result = match &config.resume_session_id {
                Some(session_id) => {
                    registry_resume(
                        self.ctx.clone(),
                        Arc::clone(&self.sessions),
                        Arc::clone(&self.tools),
                        Arc::clone(&self.llm),
                        Arc::clone(&self.registry),
                        Arc::clone(&self.live),
                        Arc::clone(&self.scopes),
                        Arc::clone(&self.system_prompts),
                        self.persistence.clone(),
                        &self.preset_registry,
                        starriver_kernel::agent::ResumeAgentOptions {
                            resume_session_id: SessionId(session_id.clone()),
                            cwd: config.cwd.clone(),
                            agent_options,
                        },
                    )
                    .await
                }
                None => {
                    self.publish(CreateAgentOptions {
                        session_id: SessionId(config.id.clone()),
                        seed: Vec::new(),
                        cwd: config.cwd.clone(),
                        agent_options,
    agent_preset: None,
        })
                    .await
                }
            };
            match result {
                Ok(handle) => handles.push(handle),
                Err(error) => tracing::warn!(
                    id = %config.id,
                    error = %error,
                    "configured agent failed to start"
                ),
            }
        }
        handles
    }
}

#[allow(clippy::too_many_arguments)]
async fn publish_agent(
    ctx: Context,
    sessions: Arc<dyn SessionStoreView>,
    tools: Arc<dyn ToolRuntimeView>,
    llm: Arc<dyn LlmRuntimeView>,
    registry: Arc<dyn AgentRegistry>,
    live: LiveAgents,
    scopes: Arc<starriver_kernel::agent::AgentScopeRegistry>,
    system_prompts: Arc<SystemPromptRegistry>,
    options: CreateAgentOptions,
) -> Result<AgentHandle, AgentError> {
    let preparation = sessions
        .prepare(
            options.session_id.clone(),
            PrepareOptions {
                cwd: options.cwd.clone(),
                seed: options.seed.clone(),
                agent_preset: options.agent_preset.clone(),
            },
        )
        .map_err(|error| AgentError::Other(error.to_string()))?;
    let session = preparation.session.clone();
    publish_agent_with_session(PublishStep {
        ctx,
        sessions,
        tools,
        llm,
        registry,
        live,
        scopes,
        system_prompts,
        session,
        agent_options: options.agent_options,
        source: SessionStartSource::Startup,
    })
    .await
}

async fn publish_agent_with_session(step: PublishStep) -> Result<AgentHandle, AgentError> {
    let PublishStep {
        ctx,
        sessions,
        tools,
        llm,
        registry,
        live,
        scopes,
        system_prompts,
        session,
        agent_options,
        source,
    } = step;
    publish_agent_with_session_core(
        ctx,
        sessions,
        tools,
        llm,
        registry,
        live,
        scopes,
        system_prompts,
        session,
        agent_options,
        source,
    )
    .await
}

/// Everything `publish_agent_with_session` needs; grouped so the hot path
/// stays under clippy's argument-count limit.
struct PublishStep {
    ctx: Context,
    sessions: Arc<dyn SessionStoreView>,
    tools: Arc<dyn ToolRuntimeView>,
    llm: Arc<dyn LlmRuntimeView>,
    registry: Arc<dyn AgentRegistry>,
    live: LiveAgents,
    scopes: Arc<starriver_kernel::agent::AgentScopeRegistry>,
    system_prompts: Arc<SystemPromptRegistry>,
    session: Session,
    agent_options: AgentOptions,
    source: SessionStartSource,
}

#[allow(clippy::too_many_arguments)]
async fn publish_agent_with_session_core(
    ctx: Context,
    sessions: Arc<dyn SessionStoreView>,
    tools: Arc<dyn ToolRuntimeView>,
    llm: Arc<dyn LlmRuntimeView>,
    registry: Arc<dyn AgentRegistry>,
    live: LiveAgents,
    scopes: Arc<starriver_kernel::agent::AgentScopeRegistry>,
    system_prompts: Arc<SystemPromptRegistry>,
    session: Session,
    agent_options: AgentOptions,
    source: SessionStartSource,
) -> Result<AgentHandle, AgentError> {
    let visible_tools = agent_options.visible_tools.clone();
    let agent_id = session.id.clone();
    let tools_for_agent = Arc::clone(&tools);
    let agent = ReactLoopAgent::new(
        ctx.clone(),
        agent_id.clone(),
        agent_options,
        session.clone(),
        llm,
        tools_for_agent,
        system_prompts,
    );
    let agent_dyn: Arc<dyn Agent> = agent.clone();
    // Register the agent scope for tools/* event routing and apply the
    // visible-tools whitelist as a per-agent restriction.
    scopes.register(agent_id.0.clone(), agent.ctx().clone());
    if let Some(visible) = visible_tools {
        if let Err(error) = tools.restrict_for(&agent_id.0, visible) {
            tracing::warn!(
                agent = %agent_id.0,
                error = %error,
                "visible-tools restriction rejected"
            );
        }
    }

    let session_handle = sessions
        .enter(session.clone())
        .map_err(|error| AgentError::Other(error.to_string()))?;
    let registry_handle = registry
        .enter(Arc::clone(&agent_dyn), None)
        .map_err(|error| AgentError::Other(error.to_string()))?;

    let result = async {
        sessions
            .announce(&session)
            .map_err(|error| AgentError::Other(error.to_string()))?;
        registry.announce(&agent_dyn)?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        registry_handle.dispose_async().await;
        session_handle.dispose();
        return Err(error);
    }

    if let Some(bus) = ctx.event_bus() {
        bus.emit_scoped(
            agent.ctx(),
            Arc::new(AgentSessionStart {
                agent: Arc::clone(&agent_dyn),
                source,
            }),
        );
    }
    agent.start();

    // Track a stop capability for factory teardown; the agent scope is
    // unwound by cancelling the driver, awaiting quiescence, and disposing
    // effects registered on the agent context.
    let stop_agent = Arc::clone(&agent_dyn);
    let stop_ctx = agent.ctx().clone();
    let stop: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let agent = Arc::clone(&stop_agent);
        let ctx = stop_ctx.clone();
        tokio::spawn(async move {
            agent.cancel(
                starriver_kernel::agent::AgentCancelCause::new(
                    starriver_kernel::agent::AgentCancelKind::Disposed,
                ),
                false,
            );
            agent.when_idle().await;
            ctx.dispose_effects().await;
        });
    });
    live.lock().unwrap().push(Arc::clone(&stop));

    let detach_registry = registry_handle;
    let detach_session = session_handle;
    let dispose_agent = Arc::clone(&agent_dyn);
    let dispose_ctx = agent.ctx().clone();
    let scope_id = session.id.0.clone();
    let scopes = Arc::clone(&scopes);
    Ok(AgentHandle::new(
        agent_dyn,
        Box::new(move || {
            let dispose_agent = Arc::clone(&dispose_agent);
            let dispose_ctx = dispose_ctx.clone();
            tokio::spawn(async move {
                dispose_agent.cancel(
                    starriver_kernel::agent::AgentCancelCause::new(
                        starriver_kernel::agent::AgentCancelKind::Disposed,
                    ),
                    false,
                );
                dispose_agent.when_idle().await;
                dispose_ctx.dispose_effects().await;
                detach_registry.dispose_async().await;
                detach_session.dispose();
                scopes.unregister(&scope_id);
            });
        }),
    ))
}

impl AgentFactory for AgentLoop {
    fn create(
        &self,
        _owner: Context,
        options: CreateAgentOptions,
    ) -> BoxFuture<Result<AgentHandle, AgentError>> {
        let ctx = self.ctx.clone();
        let sessions = Arc::clone(&self.sessions);
        let tools = Arc::clone(&self.tools);
        let llm = Arc::clone(&self.llm);
        let registry = Arc::clone(&self.registry);
        let live = Arc::clone(&self.live);
        let scopes = Arc::clone(&self.scopes);
        let system_prompts = Arc::clone(&self.system_prompts);
        let options = self.resolve_create_options(options);
        Box::pin(async move {
            publish_agent(
                ctx,
                sessions,
                tools,
                llm,
                registry,
                live,
                scopes,
                system_prompts,
                options,
            )
            .await
        })
    }

    fn resume(
        &self,
        _owner: Context,
        options: ResumeAgentOptions,
    ) -> BoxFuture<Result<AgentHandle, AgentError>> {
        let persistence = self.persistence.clone();
        let ctx = self.ctx.clone();
        let sessions = Arc::clone(&self.sessions);
        let tools = Arc::clone(&self.tools);
        let llm = Arc::clone(&self.llm);
        let registry = Arc::clone(&self.registry);
        let live = Arc::clone(&self.live);
        let scopes = Arc::clone(&self.scopes);
        let system_prompts = Arc::clone(&self.system_prompts);
        let preset_registry = self.preset_registry.clone();
        Box::pin(async move {
            registry_resume(
                ctx,
                sessions,
                tools,
                llm,
                registry,
                live,
                scopes,
                system_prompts,
                persistence,
                &preset_registry,
                options,
            )
            .await
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn registry_resume(
    ctx: Context,
    sessions: Arc<dyn SessionStoreView>,
    tools: Arc<dyn ToolRuntimeView>,
    llm: Arc<dyn LlmRuntimeView>,
    registry: Arc<dyn AgentRegistry>,
    live: LiveAgents,
    scopes: Arc<starriver_kernel::agent::AgentScopeRegistry>,
    system_prompts: Arc<SystemPromptRegistry>,
    persistence: Option<Arc<dyn PersistenceView>>,
    preset_registry: &Arc<std::sync::Mutex<std::collections::HashMap<String, starriver_kernel::agent::AgentOptions>>>,
    options: starriver_kernel::agent::ResumeAgentOptions,
) -> Result<AgentHandle, AgentError> {
    let Some(persistence) = persistence else {
        return Err(AgentError::NoPersistence);
    };
    let id = options.resume_session_id;
    let preparation = match persistence.prepare(id.clone(), None).await {
        Ok(preparation) => preparation,
        Err(starriver_session_persistence::prepare::PrepareError::NotFound) => {
 // NotFound falls back to create (aligned with
            // restoreOrCreateConfigured)
            let create_options = CreateAgentOptions {
                session_id: id,
                seed: Vec::new(),
                cwd: options.cwd.clone(),
                // The fallback create has no header yet; resume composition
                // is undefined for it (the preset rides the stored header).
                agent_preset: None,
                agent_options: options.agent_options,
            };
            return publish_agent(
                ctx,
                sessions,
                tools,
                llm,
                registry,
                live,
                scopes,
                system_prompts,
                create_options,
            )
            .await;
        }
        Err(error) => return Err(AgentError::Other(error.to_string())),
    };
    let header_preset = preparation.session.header.agent_preset.clone();
    // Lock scoped: a guard alive across the publish await makes the future
    // !Send (the generator keeps guard state in its stash).
    let agent_options = {
        let registry_map = preset_registry.lock().unwrap_or_else(|e| e.into_inner());
        resolve_preset_options(
            &registry_map,
            header_preset.as_deref(),
            options.agent_options,
        )
    };
    let session = preparation.session.clone();
    let handle = publish_agent_with_session(PublishStep {
        ctx,
        sessions,
        tools,
        llm,
        registry,
        live,
        scopes,
        system_prompts,
        session,
        agent_options,
        source: SessionStartSource::Resume,
    })
    .await?;
    preparation.attach();
    Ok(handle)
}

impl Drop for AgentLoop {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_rslite::{Context, EventBus, EventError};
    use starriver_kernel::agent::{AgentError, CreateAgentOptions};
    use starriver_kernel::session::{SessionCreated, SessionId};

    fn store_on(bus: EventBus) -> Arc<dyn SessionStoreView> {
        let ctx = Context::new().with_event_bus(&bus);
        Arc::new(starriver_session::InMemorySessionStore::new(bus, ctx))
    }

    fn registry_arc() -> Arc<dyn AgentRegistry> {
        Arc::new(crate::registry::InMemoryAgentRegistry::new())
    }

    #[tokio::test]
    async fn create_publishes_agent_and_session() {
        use starriver_kernel::agent::AgentOptions;

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let sessions = store_on(bus.clone());
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn ToolRuntimeView>;
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(EventBus::new()))
            as Arc<dyn LlmRuntimeView>;
        let registry = registry_arc();
        let loop_service = AgentLoop::new(
            ctx,
            AgentLoopConfig::default(),
            Arc::clone(&sessions),
            tools,
            llm,
            Arc::clone(&registry),
            None,
            std::collections::HashMap::new(),
            Arc::new(starriver_kernel::agent::AgentScopeRegistry::new()),
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        let _factory = registry.set_factory(Arc::new(loop_service)).unwrap();

        let handle = registry
            .create(CreateAgentOptions {
                session_id: SessionId("a1".into()),
                seed: Vec::new(),
                cwd: None,
                agent_preset: None,
                agent_options: AgentOptions {
                    compaction_tuning: None,
                    provider: Some("mock".into()),
                    model: Some("m1".into()),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        assert_eq!(handle.agent.id().0, "a1");
        assert!(registry.get(&SessionId("a1".into())).is_some());
        assert!(sessions.get(&SessionId("a1".into())).is_some());

        handle.dispose();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(registry.get(&SessionId("a1".into())).is_none());
        assert!(sessions.get(&SessionId("a1".into())).is_none());
    }

    #[tokio::test]
    async fn start_configured_resumes_when_resume_session_id_present() {
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let sessions = store_on(bus.clone());
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn ToolRuntimeView>;
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(EventBus::new()))
            as Arc<dyn LlmRuntimeView>;
        let registry = registry_arc();
        let persistence = Arc::new(
            starriver_session_persistence::SqliteSessionPersistence::open(
                std::env::temp_dir().join(format!("sc-{}.db", uuid::Uuid::new_v4())),
            )
            .unwrap(),
        );
        let loop_service = Arc::new(AgentLoop::new(
            ctx,
            AgentLoopConfig {
                max_parallel_tool_calls: 10,
                agents: vec![AgentConfig {
                    id: "a1".into(),
                    provider: Some("mock".into()),
                    model: Some("m1".into()),
                    max_tokens: Some(512),
                    cwd: None,
                    resume_session_id: Some("s1".into()),
                }],
            },
            Arc::clone(&sessions),
            tools,
            llm,
            Arc::clone(&registry),
            Some(Arc::clone(&persistence) as Arc<dyn PersistenceView>),
            std::collections::HashMap::new(),
            Arc::new(starriver_kernel::agent::AgentScopeRegistry::new()),
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        ));
        let _factory =
            registry
                .set_factory(
                    Arc::clone(&loop_service) as Arc<dyn starriver_kernel::agent::AgentFactory>
                )
                .unwrap();
        // Resume a missing session falls back to create; the agent must exist.
        let handles = loop_service.start_configured().await;
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].agent.id().0, "s1");
    }

    #[test]
    fn agent_options_from_preset_then_overridden() {
        let mut preset_registry = std::collections::HashMap::new();
        preset_registry.insert(
            "coding".to_string(),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("deepseek".into()),
                model: Some("coder".into()),
                max_tokens: Some(4096),
                system: Some("You are a coding agent.".into()),
                ..Default::default()
            },
        );
        let options = resolve_preset_options(
            &preset_registry,
            Some("coding"),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("override".into()),
                ..Default::default()
            },
        );
        assert_eq!(options.provider.as_deref(), Some("override"));
        assert_eq!(options.model.as_deref(), Some("coder"));
        assert_eq!(options.max_tokens, Some(4096));
        assert_eq!(options.system.as_deref(), Some("You are a coding agent."));
    }

    #[test]
    fn missing_preset_keeps_caller_options() {
        let preset_registry = std::collections::HashMap::new();
        let caller = starriver_kernel::agent::AgentOptions {
            compaction_tuning: None,
            provider: Some("p".into()),
            model: Some("m".into()),
            max_tokens: Some(1),
            system: None,
            ..Default::default()
        };
        let options = resolve_preset_options(&preset_registry, Some("nope"), caller.clone());
        assert_eq!(options.provider, caller.provider);
        assert_eq!(options.model, caller.model);
        assert_eq!(options.max_tokens, caller.max_tokens);
        assert_eq!(options.system, caller.system);
    }

    /// WI-5: a fresh session naming a preset composes from the shared
    /// registry immediately (create-path resolution, symmetric with resume).
    #[tokio::test]
    async fn create_resolves_agent_preset_from_the_shared_registry() {
        use starriver_kernel::agent::AgentFactory as _;

        let ctx = Context::new().with_event_bus(&EventBus::new());
        let preset_registry = std::sync::Mutex::new(std::collections::HashMap::new());
        preset_registry
            .lock()
            .unwrap()
            .insert("coder".to_string(), AgentOptions { max_tokens: Some(777), ..Default::default() });
        let sessions = store_on(EventBus::new());
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn ToolRuntimeView>;
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(EventBus::new()))
            as Arc<dyn LlmRuntimeView>;
        let loop_service = AgentLoop::new_shared(
            ctx.clone(),
            AgentLoopConfig::default(),
            sessions,
            tools,
            llm,
            registry_arc(),
            None,
            Arc::new(preset_registry),
            Arc::new(starriver_kernel::agent::AgentScopeRegistry::new()),
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        let handle = loop_service
            .create(
                ctx,
                CreateAgentOptions {
                    session_id: SessionId("s-preset".into()),
                    seed: vec![],
                    cwd: None,
                    agent_preset: Some("coder".into()),
                    agent_options: AgentOptions::default(),
                },
            )
            .await
            .unwrap();
        assert_eq!(handle.agent.options().max_tokens, Some(777));
        assert_eq!(
            handle.agent.session().header.agent_preset.as_deref(),
            Some("coder")
        );
    }

    fn resolve_preset_options_inherits_tools_and_sections() {
        let mut preset_registry = std::collections::HashMap::new();
        preset_registry.insert(
            "coding".to_string(),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("deepseek".into()),
                model: Some("coder".into()),
                max_tokens: Some(4096),
                system: None,
                system_sections: vec![("role".into(), "coder".into())],
                visible_tools: Some(vec!["bash".into()]),
                time_refresh_interval_ms: Some(60_000),
                prompt_settings: None,
                context_window: None,
                compaction_threshold_ratio: None,
                compaction_retain_ratio: None,
                image_projection: None,
                image_resolver: None,
                mcp_status_feed: None,
                max_parallel_tool_calls: None,
                turn_tool_call_budget: None,
            },
        );
        let options = resolve_preset_options(
            &preset_registry,
            Some("coding"),
            starriver_kernel::agent::AgentOptions::default(),
        );
        assert_eq!(
            options.visible_tools.as_deref(),
            Some(&["bash".to_string()][..])
        );
        assert_eq!(options.system_sections.len(), 1);
    }

    /// WI-163: the availability feed is runtime state — a preset value must
    /// not override the caller's live feed.
    #[test]
    fn resolve_preset_options_caller_feed_wins_over_preset() {
        let mut preset_registry = std::collections::HashMap::new();
        preset_registry.insert(
            "coding".to_string(),
            starriver_kernel::agent::AgentOptions {
                mcp_status_feed: Some(starriver_kernel::agent::SharedMcpStatusFeed(
                    std::sync::Arc::new(|| vec![("preset".to_string(), "x".to_string())]),
                )),
                ..Default::default()
            },
        );
        let caller = starriver_kernel::agent::AgentOptions {
            mcp_status_feed: Some(starriver_kernel::agent::SharedMcpStatusFeed(
                std::sync::Arc::new(|| vec![("caller".to_string(), "y".to_string())]),
            )),
            ..Default::default()
        };
        let merged = resolve_preset_options(&preset_registry, Some("coding"), caller);
        let feed = merged.mcp_status_feed.expect("feed survives the merge");
        assert_eq!((feed.0)()[0].0, "caller");
    }

    #[tokio::test]
    async fn publish_registers_scope_and_dispose_unregisters() {
        use starriver_kernel::agent::AgentScopeRegistry;

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let sessions = store_on(bus.clone());
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn ToolRuntimeView>;
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(EventBus::new()))
            as Arc<dyn LlmRuntimeView>;
        let registry = registry_arc();
        let scopes = Arc::new(AgentScopeRegistry::new());
        let loop_service = Arc::new(AgentLoop::new(
            ctx,
            AgentLoopConfig::default(),
            Arc::clone(&sessions),
            tools,
            llm,
            Arc::clone(&registry),
            None,
            std::collections::HashMap::new(),
            Arc::clone(&scopes),
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        ));
        let _factory =
            registry
                .set_factory(
                    Arc::clone(&loop_service) as Arc<dyn starriver_kernel::agent::AgentFactory>
                )
                .unwrap();
        let handle = registry
            .create(CreateAgentOptions {
                session_id: SessionId("s1".into()),
                seed: Vec::new(),
                cwd: None,
                agent_options: starriver_kernel::agent::AgentOptions::default(),
    agent_preset: None,
        })
            .await
            .unwrap();
        assert!(
            scopes.lookup("s1").is_some(),
            "publish must register the agent scope"
        );
        handle.dispose();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            scopes.lookup("s1").is_none(),
            "dispose must unregister the agent scope"
        );
    }

    #[tokio::test]
    async fn session_created_veto_rolls_back_publication() {
        use starriver_kernel::agent::AgentOptions;

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let sessions = store_on(bus.clone());
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn ToolRuntimeView>;
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(EventBus::new()))
            as Arc<dyn LlmRuntimeView>;
        let registry = registry_arc();
        let loop_service = AgentLoop::new(
            ctx.clone(),
            AgentLoopConfig::default(),
            Arc::clone(&sessions),
            tools,
            llm,
            Arc::clone(&registry),
            None,
            std::collections::HashMap::new(),
            Arc::new(starriver_kernel::agent::AgentScopeRegistry::new()),
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        let _factory = registry.set_factory(Arc::new(loop_service)).unwrap();

        let _veto = bus.on_sync::<SessionCreated, _>(|_| Err(EventError::Listener("veto".into())));

        let error = match registry
            .create(CreateAgentOptions {
                session_id: SessionId("a2".into()),
                seed: Vec::new(),
                cwd: None,
                agent_options: AgentOptions::default(),
    agent_preset: None,
        })
            .await
        {
            Ok(_) => panic!("expected veto to fail publication"),
            Err(error) => error,
        };
        assert!(matches!(error, AgentError::Other(_)));
        assert!(registry.get(&SessionId("a2".into())).is_none());
        assert!(sessions.get(&SessionId("a2".into())).is_none());
    }

    #[tokio::test]
    async fn dispose_runs_agent_scope_effects() {
        use starriver_kernel::agent::AgentOptions;
        use std::sync::atomic::{AtomicBool, Ordering};

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let sessions = store_on(bus.clone());
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn ToolRuntimeView>;
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(EventBus::new()))
            as Arc<dyn LlmRuntimeView>;
        let registry = registry_arc();
        let loop_service = AgentLoop::new(
            ctx,
            AgentLoopConfig::default(),
            Arc::clone(&sessions),
            tools,
            llm,
            Arc::clone(&registry),
            None,
            std::collections::HashMap::new(),
            Arc::new(starriver_kernel::agent::AgentScopeRegistry::new()),
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        let _factory = registry.set_factory(Arc::new(loop_service)).unwrap();
        let handle = registry
            .create(CreateAgentOptions {
                session_id: SessionId("s1".into()),
                seed: Vec::new(),
                cwd: None,
                agent_options: AgentOptions::default(),
    agent_preset: None,
        })
            .await
            .unwrap();

        let unwound = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&unwound);
        let _scope_effect = handle.agent.ctx().effect(move || {
            flag.store(true, Ordering::SeqCst);
        });

        handle.dispose();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            unwound.load(Ordering::SeqCst),
            "dispose must unwind effects registered on the agent scope"
        );
    }

    #[tokio::test]
    async fn factory_drop_stops_live_agents() {
        use starriver_kernel::agent::AgentOptions;
        use std::sync::atomic::{AtomicBool, Ordering};

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let sessions = store_on(bus.clone());
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn ToolRuntimeView>;
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(EventBus::new()))
            as Arc<dyn LlmRuntimeView>;
        let registry = registry_arc();
        let loop_service = Arc::new(AgentLoop::new(
            ctx,
            AgentLoopConfig::default(),
            Arc::clone(&sessions),
            tools,
            llm,
            Arc::clone(&registry),
            None,
            std::collections::HashMap::new(),
            Arc::new(starriver_kernel::agent::AgentScopeRegistry::new()),
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        ));
        let factory_handle =
            registry
                .set_factory(
                    Arc::clone(&loop_service) as Arc<dyn starriver_kernel::agent::AgentFactory>
                )
                .unwrap();
        let handle = registry
            .create(CreateAgentOptions {
                session_id: SessionId("f1".into()),
                seed: Vec::new(),
                cwd: None,
                agent_options: AgentOptions::default(),
    agent_preset: None,
        })
            .await
            .unwrap();

        let unwound = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&unwound);
        let _scope_effect = handle.agent.ctx().effect(move || {
            flag.store(true, Ordering::SeqCst);
        });

        // Remove the factory reference, then drop the loop service: its Drop
        // must stop and unwind every live agent.
        factory_handle.dispose_async().await;
        drop(loop_service);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            unwound.load(Ordering::SeqCst),
            "factory teardown must stop live agents and unwind their scopes"
        );
    }

    // ── SP2: sandbox:policy runtime-context contributor ─────────────────────

    fn mode_event(session: &Session, mode: &str) {
        use starriver_kernel::json::JsonValue;
        session
            .append(
                "sandbox/mode",
                JsonValue::Object(vec![("mode".into(), JsonValue::String(mode.into()))]),
            )
            .unwrap();
    }

    #[test]
    fn sandbox_policy_renders_mode_with_writable_root() {
        let mut session = Session::new(SessionId("sp".into()), EventBus::new());
        mode_event(&session, "workspace-write");
        session.header.cwd = Some("/w/proj".into());
        let text = sandbox_policy_text(&session);
        assert!(
            text.contains("workspace-write") && text.contains("/w/proj"),
            "writable root from the frozen header must appear: {text}"
        );
    }

    #[test]
    fn sandbox_policy_empty_without_a_mode_event() {
        let session = Session::new(SessionId("sp".into()), EventBus::new());
        assert!(
            sandbox_policy_text(&session).is_empty(),
            "no mode event → no contribution (join drops empty sections)"
        );
    }

    #[test]
    fn policy_contexts_order_sandbox_before_approval() {
        // Registration order mirrors AgentLoop::new; the order field decides.
        let registry = Arc::new(SystemPromptRegistry::new());
        let ctx = Context::new().with_event_bus(&EventBus::new());
        registry
            .context(
                &ctx,
                starriver_kernel::context::PromptContext {
                    name: "approval:policy".into(),
                    order: 115,
                    render: Arc::new(|_| "approval".into()),
                },
            )
            .unwrap();
        registry
            .context(
                &ctx,
                starriver_kernel::context::PromptContext {
                    name: "sandbox:policy".into(),
                    order: 110,
                    render: Arc::new(|_| "sandbox".into()),
                },
            )
            .unwrap();
        let agent_ctx = ctx.clone().tagged(Arc::from("s1"));
        let store = starriver_session::InMemorySessionStore::new(EventBus::new(), Context::new());
        let preparation = store
            .prepare(
                SessionId("s1".into()),
                PrepareOptions {
                    cwd: Some("/w/proj".into()),
                    seed: Vec::new(),
            agent_preset: None,
        },
            )
            .unwrap();
        let render = starriver_kernel::context::RenderContext {
            session: &preparation.session,
            options: &AgentOptions::default(),
        };
        let out = registry.assemble(&agent_ctx, &render).unwrap();
        let names: Vec<&str> = out.contexts.iter().map(|c| c.name.as_str()).collect();
        let sandbox = names.iter().position(|n| *n == "sandbox:policy").unwrap();
        let approval = names.iter().position(|n| *n == "approval:policy").unwrap();
        assert!(sandbox < approval, "order 110 must precede 115: {names:?}");
    }
}
