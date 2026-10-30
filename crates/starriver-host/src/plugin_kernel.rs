//! Plugin agent kernel: composes the new agent-loop registry, loop service,
//! and SQLite persistence into the execute boundary the host uses. Replaces
//! the legacy loop engine on the host main path.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use cordis_rslite::EffectHandle;
use starriver_kernel::agent::{
    Agent, AgentFactory, AgentOptions, AgentRegistry, AgentRequest, AgentScopeRegistry,
    CreateAgentOptions, ResumeAgentOptions,
};
use starriver_kernel::llm::LlmRuntime;
use starriver_kernel::llm::{ContentBlock, Message, MessageId, MessageRole, MessageSource};
use starriver_kernel::session::SessionId;
use starriver_kernel::system_prompt::{SectionEntry, SectionText, SystemPromptRegistry};
use starriver_kernel::tools::{
    PostToolDecision, PreToolDecision, ToolDefinition, ToolExecutionResult, ToolRuntime,
    ToolsPostExecute, ToolsPreExecute,
};
use starriver_kernel::ui::UiEvent;

/// Test hook: swap web tool definitions so engine-level e2e can point at
/// local mock servers without touching the SSRF guard / search endpoints.
#[derive(Default)]
pub struct WebToolOverrides {
    pub web_fetch: Option<ToolDefinition>,
    pub web_search: Option<ToolDefinition>,
}

/// 逐字节实时一致（开放回合快照流）：由 host 注入的快照上下文。
/// `inner` 走与 session.trace 完全相同的账本读取路径（load_projection_parts）；
/// `epochs` 是 per-session 投影世代表（compaction/fork/回滚时递增，中继亦会
/// 在见到 Compaction 事件时递增）。
#[derive(Clone)]
pub struct TraceSnapContext {
    pub inner: Arc<tokio::sync::Mutex<Option<crate::RuntimeInner>>>,
    pub epochs: Arc<StdMutex<HashMap<String, u64>>>,
}

/// 每次运行的快照源（id 在 execute_with_meta_in 内才确定，故运行时构建）。
struct TraceSnapSource {
    session_id: String,
    ctx: TraceSnapContext,
    last_hash: Option<u64>,
}

impl TraceSnapSource {
    fn epoch(&self) -> u64 {
        *self
            .ctx
            .epochs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&self.session_id)
            .unwrap_or(&0)
    }

    fn bump_epoch(&self) {
        let mut map = self.ctx.epochs.lock().unwrap_or_else(|e| e.into_inner());
        let next = map.get(&self.session_id).copied().unwrap_or(0) + 1;
        map.insert(self.session_id.clone(), next);
    }

    /// 结构事件后计算开放回合快照：无 user 条目（回合未开始）或内容与上次
    /// 相同 → None。
    async fn compute(&mut self) -> Option<UiEvent> {
        let inner = self.ctx.inner.clone();
        let session_id = self.session_id.clone();
        let l = inner.lock().await;
        let runtime = l.as_ref()?;
        let (entries, tool_names) = crate::sessions::load_projection_parts(runtime, &session_id)
            .await
            .ok()?;
        let (turn_index, slice) = crate::sessions::open_turn_slice(entries)?;
        let epoch = self.epoch();
        let event = crate::sessions::build_trace_snapshot(
            &session_id,
            epoch,
            turn_index,
            slice,
            &tool_names,
        )?;
        // 相同内容跳过：对序列化文本做哈希。
        use std::hash::{Hash, Hasher};
        let text = serde_json::to_string(&event).ok()?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        let hash = h.finish();
        if self.last_hash == Some(hash) {
            return None;
        }
        self.last_hash = Some(hash);
        Some(event)
    }
}

/// Host-supplied session metadata appended to the session log at create.
#[derive(Debug, Clone, Default)]
pub struct HostSessionMeta {
    /// Display title; the host derives it from the task when absent.
    pub title: Option<String>,
    /// dsh-pattern per-model declaration: `true` when the activated variant
    /// declares itself text-only (`supports_vision: false` in settings).
    /// Request assembly then rejects any image block with an explicit error
    /// naming the model, instead of putting image data on the wire.
    pub reject_images: bool,
}

/// Per-server MCP tool registrations on the kernel seam.
#[derive(Default)]
struct McpToolRegistry {
    by_server: HashMap<String, Vec<(String, EffectHandle)>>,
}

/// `-100` identity resident: the fixed platform sentence every agent carries.
pub const IDENTITY_SECTION: &str = "identity";
/// `0` persona resident: the active agent profile body (global slot; replaced
/// per switch — single-active-agent semantics for now).
pub const PERSONA_SECTION: &str = "deployment:persona";
/// `60` skills resident: always-on skill bodies, re-evaluated every assembly.
pub const SKILLS_SECTION: &str = "skills:always-on";
/// `80` review resident: complete:true whole-prompt replacement while active.
pub const REVIEW_SECTION: &str = "mode:review";

pub const IDENTITY_PROMPT: &str = "You are an AI agent powered by Starriver.";

/// Text of the `-100` identity resident for one agent: suppressed when its
/// prompt settings opt out (empty sections drop out of the assembly).
pub fn identity_section_text(
    settings: Option<&starriver_kernel::system_prompt::PromptSettings>,
) -> String {
    match settings {
        Some(settings) if !settings.include_identity => String::new(),
        _ => IDENTITY_PROMPT.to_string(),
    }
}

/// Text of the `-99` harness:source resident (includeHarnessSource
/// parity, wording per prompt-parity B.1); unset → no section.
pub fn source_section_text(
    settings: Option<&starriver_kernel::system_prompt::PromptSettings>,
) -> String {
    match settings.and_then(|settings| settings.source_path.as_deref()) {
        Some(path) => {
            format!("Source checkout: {path} — do not infer the working directory from this path.")
        }
        None => String::new(),
    }
}

/// Text of the `deployment:baseline` resident: the platform-level persona
/// from settings; unset → no section.
pub fn persona_global_section_text(
    settings: Option<&starriver_kernel::system_prompt::PromptSettings>,
) -> String {
    settings
        .and_then(|settings| settings.persona_global.clone())
        .unwrap_or_default()
}

/// The `coding` preset composed from CURRENT settings (backlog #7 settings
/// hot-update seam). Called at startup for the loop's frozen registry AND
/// per-run in `default_agent_options` — a settings write takes effect on the
/// next run without a restart.
pub fn coding_preset(settings: &crate::settings::Settings) -> AgentOptions {
    let tuning = settings.agent.compaction_tuning();
    AgentOptions {
        compaction_tuning: tuning,
        provider: Some("mock".into()),
        model: Some("mock".into()),
        // Output budget floor. 8192 truncated interleaved-thinking
        // models mid-reasoning (finish reason max-tokens) before any
        // tool call was emitted — the turn burned its budget on
        // thinking alone, which PTC mode amplifies (the model reasons
        // about the whole program up front). 32k covers thinking plus
        // a full program; an explicit per-provider max_tokens (model
        // settings UI) still outranks this floor.
        max_tokens: Some(32 * 1024),
        system: Some("You are a coding agent.".into()),
        system_sections: vec![],
        // SP4: time-context sampling — settings override, 60s default.
        time_refresh_interval_ms: settings.agent.time_refresh_interval_ms.or(Some(60_000)),
        // LLP settings: identity on, runtime-context on, no source
        // section, no global persona (per-agent persona is injected
        // by switch_agent). Deployments override via prompt_settings.
        prompt_settings: Some(starriver_kernel::system_prompt::PromptSettings {
            include_identity: settings.prompt.include_identity,
            include_runtime_context: settings.prompt.include_runtime_context,
            source_path: settings.prompt.source_path.clone(),
            persona_global: settings.prompt.persona_global.clone(),
            instructions_max_bytes: settings.prompt.instructions_max_bytes,
            response_language: settings.prompt.response_language.clone(),
            tool_order: settings.prompt.tool_order.clone(),
        }),
        context_window: settings.agent.context_window,
        compaction_threshold_ratio: settings.agent.compaction_threshold_ratio,
        compaction_retain_ratio: settings.agent.compaction_retain_ratio,
        // WI-199: same-step escalation ladder rung ③ — fallback route
        // ("provider/model"). A malformed route (no "/" or empty halves) is
        // ignored; settings validation surfaces typos in the UI.
        fallback_provider: settings
            .agent
            .fallback_route
            .as_deref()
            .and_then(|route| route.split_once('/'))
            .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
            .map(|(provider, model)| provider.to_string()),
        fallback_model: settings
            .agent
            .fallback_route
            .as_deref()
            .and_then(|route| route.split_once('/'))
            .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
            .map(|(_, model)| model.to_string()),
        // Backlog #7: per-step parallel tool-call budget from settings.
        max_parallel_tool_calls: settings.agent.max_parallel_tool_calls,
        // BUG-138: per-turn tool-call budget (repeat breaker) from settings.
        turn_tool_call_budget: settings.agent.turn_tool_call_budget,
        // WI-1: projection config/resolver are injected per-run in
        // default_agent_options (post-preset-merge), so the preset stays None.
        image_projection: None,
        image_resolver: None,
        // WI-163: same per-run injection for the MCP availability feed.
        mcp_status_feed: None,
        // No hardcoded visible-tools whitelist: EVERY registered tool
        // is presented (registry contract). The list drifted the
        // moment new tools landed — git/terminal/todo/python_exec
        // were registered but invisible for months. Governance stays
        // where it belongs: sandbox gates + approval on the tools
        // themselves; the review whitelist and MCP projection still
        // flow through visible_tools below.
        visible_tools: None,
    }
}

/// Register the settings-gated residents (identity / harness:source /
/// deployment:baseline). They render from each agent's AgentOptions
/// (prompt_settings) at assembly time — one registration, per-agent honor; a
/// section that evaluates to empty drops out of the assembly entirely.
/// Text of the `language` resident (order 10, `language`-section
/// parity): follow the user's message language by default; a configured
/// `prompt.response_language` pins all user-facing output to that language.
pub fn language_section_text(
    settings: Option<&starriver_kernel::system_prompt::PromptSettings>,
) -> String {
    match settings.and_then(|settings| settings.response_language.as_deref()) {
        Some(lang) if !lang.trim().is_empty() => format!(
            "Write all user-facing replies and plan documents in {lang}. \
             Quote code, identifiers and error output verbatim."
        ),
        _ => "Write user-facing replies in the language of the user's messages.".into(),
    }
}

/// Text of the `communication` resident (order 20): the working-agreement
/// rules spread across Tone and style / Delivering work /
/// Corrections / act-dont-rederive, merged into one section. Registration is
/// unconditional — these are platform-level norms, not per-agent persona.
pub const COMMUNICATION_SECTION: &str = "# Communicating & working\n\n\
- Be concise and direct; lead with the outcome, supporting detail after.\n\
- Only use emojis if the user uses them.\n\
- Reference code as `file_path:line_number` — it is clickable in the UI.\n\
- The requested scope is the deliverable: do not quietly narrow, widen or \
transform it; surface surprises instead of absorbing them.\n\
- When you have enough information to act, act. Do not re-derive facts the \
conversation has already established.\n\
- Report outcomes faithfully: failing tests go with their output, skipped \
steps are named, and finished work is stated plainly without hedging.\n\
- Do not announce a tool call with a trailing colon; just proceed.\n\
- Avoid excessive self-correction: adjust once and move on.";

pub fn register_settings_residents(
    system_prompts: &SystemPromptRegistry,
    ctx: &cordis_rslite::Context,
) -> Vec<EffectHandle> {
    let identity = system_prompts
        .section(
            ctx,
            SectionEntry {
                name: IDENTITY_SECTION.into(),
                order: -100,
                complete: false,
                text: SectionText::Dynamic(Arc::new(|render| {
                    identity_section_text(render.options.prompt_settings.as_ref())
                })),
            },
        )
        .ok();
    // `-99` harness:source resident (includeHarnessSource parity):
    // declares the checkout path for OSS/checkout run forms; absent
    // settings → empty → no section.
    let source = system_prompts
        .section(
            ctx,
            SectionEntry {
                name: "harness:source".into(),
                order: -99,
                complete: false,
                text: SectionText::Dynamic(Arc::new(|render| {
                    source_section_text(render.options.prompt_settings.as_ref())
                })),
            },
        )
        .ok();
    // Platform baseline persona (order 0): global rules below which the
    // per-agent persona slot (`deployment:persona`, also order 0) lands.
    // Named so it sorts before the per-agent section at equal order.
    let baseline = system_prompts
        .section(
            ctx,
            SectionEntry {
                name: "deployment:baseline".into(),
                order: 0,
                complete: false,
                text: SectionText::Dynamic(Arc::new(|render| {
                    persona_global_section_text(render.options.prompt_settings.as_ref())
                })),
            },
        )
        .ok();
    // `language` resident (order 10, parity): follow the user's
    // message language; `prompt.response_language` pins it.
    let language = system_prompts
        .section(
            ctx,
            SectionEntry {
                name: "language".into(),
                order: 10,
                complete: false,
                text: SectionText::Dynamic(Arc::new(|render| {
                    language_section_text(render.options.prompt_settings.as_ref())
                })),
            },
        )
        .ok();
    // `communication` resident (order 20): platform working-agreement rules,
    // unconditional (not part of the persona settings).
    let communication = system_prompts
        .section(
            ctx,
            SectionEntry {
                name: "communication".into(),
                order: 20,
                complete: false,
                text: SectionText::Static(COMMUNICATION_SECTION.into()),
            },
        )
        .ok();
    vec![identity, source, language, communication, baseline]
        .into_iter()
        .flatten()
        .collect()
}

/// Compose the always-on skills section exactly as the legacy
/// `inject_to_system_prompt` did (minus its leading separator — the registry
/// join supplies "\n\n" between residents).
fn always_on_skills_section(
    manager: &starriver_skills::profile::skill_manager::SkillManager,
) -> String {
    let skills = manager.always_on_skills();
    if skills.is_empty() {
        return String::new();
    }
    let mut text = String::from("## Always-Active Skills\n\n");
    for skill in &skills {
        text.push_str(&format!("### {}\n\n{}\n\n---\n\n", skill.name, skill.body));
    }
    text
}

impl McpToolRegistry {
    fn names(&self) -> Vec<String> {
        self.by_server
            .values()
            .flatten()
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// Composition root for the plugin agent path.
#[allow(dead_code)]
pub struct PluginKernel {
    ctx: cordis_rslite::Context,
    registry: Arc<starriver_agent_loop::registry::InMemoryAgentRegistry>,
    loop_service: Arc<starriver_agent_loop::agent_loop::AgentLoop>,
    persistence: Arc<starriver_session_persistence::SqliteSessionPersistence>,
    sessions: Arc<dyn starriver_agent_loop::services::SessionStoreView>,
    tools: Arc<starriver_tools::InMemoryToolRuntime>,
    /// Per-server MCP registrations on the kernel seam; handles are kept
    /// alive so `unregister_mcp_definitions` can dispose them later.
    mcp: Arc<std::sync::Mutex<McpToolRegistry>>,
    /// Concrete runtime so tests can register mock adapters.
    llm: Arc<starriver_llm::InMemoryLlmRuntime>,
    preset_registry: HashMap<String, AgentOptions>,
    scopes: Arc<AgentScopeRegistry>,
    /// Shared system-prompt registry hosting the built-in residents
    /// (identity/persona/skills/review) and runtime-context contributions.
    system_prompts: Arc<SystemPromptRegistry>,
    /// Always-on skill source for the skills resident; attached by the host
    /// once its SkillRuntime exists. None = the section renders empty. Shared
    /// with the resident's dynamic provider through this `Arc`.
    skills_cell:
        Arc<std::sync::Mutex<Option<Arc<starriver_skills::profile::skill_manager::SkillManager>>>>,
    persona_slot: std::sync::Mutex<Option<EffectHandle>>,
    /// Host-updated text for the dynamic `memory` resident (WI-66 / BUG-1):
    /// the run path writes the recalled-facts section here before spawning;
    /// an empty string makes the resident drop out of the assembly entirely.
    memory_slot: Arc<std::sync::Mutex<String>>,
    /// Host-updated text for the dynamic `task-skills` resident (BUG-6):
    /// per-run skill bodies (scheduled tasks' `skills` list, interactive
    /// slash dispatch); empty keeps the section out of the assembly.
    skill_injection_slot: Arc<std::sync::Mutex<String>>,
    /// Host-updated text for the dynamic `run:unattended` resident: scheduled
    /// runs write the unattended-contract section here (no user present —
    /// never ask, decide and state assumptions); empty keeps the section out
    /// so interactive runs are unaffected.
    unattended_slot: Arc<std::sync::Mutex<String>>,
    /// Host-updated lines for the dynamic `feedback` resident (WI-48 feedback
    /// slice): recent 👍/👎 records; empty keeps the section out entirely.
    feedback_slot: Arc<std::sync::Mutex<Vec<String>>>,
    review_slot: std::sync::Mutex<Option<EffectHandle>>,
    /// Visible-tool whitelist while review mode is active (E7 completion:
    /// the legacy dead `tool_whitelist` now really restricts the kernel).
    review_whitelist: std::sync::Mutex<Option<Vec<String>>>,
    /// Host-side approval answerer: claims `approval/request` waterfall
    /// questions and resolves them when the UI responds.
    pub approval_bridge: Arc<crate::approval_bridge::ApprovalBridge>,
    pub question_bridge: Arc<crate::question_bridge::QuestionBridge>,
    /// Permission preset pinned into every fresh session.
    preset: Arc<std::sync::Mutex<crate::preset::Preset>>,
    /// Whether `set_preset` was called explicitly: the execution mode no
    /// longer refreshes the preset (explicit override wins).
    preset_override: std::sync::atomic::AtomicBool,
    /// Current execution mode (pushed at host startup / set_execution_mode; None = unset).
    /// The execute path does not read the global settings file — avoids
    /// environment leakage, and changes take effect immediately. Shared with
    /// the plan controller so the approved exit bundles the sandbox release.
    execution_mode: Arc<std::sync::Mutex<Option<crate::execution_mode::ExecutionMode>>>,
    /// Plan fold (plan-mode parity): owns the pending `plan/mode` flip
    /// and lands it at the next accepted in-turn pre-step.
    plan_controller: Arc<crate::plan_mode::PlanModeController>,
    /// Host-side plan-review answerer backing the `exit_plan_mode` tool.
    pub plan_review_bridge: Arc<crate::plan_review_bridge::PlanReviewBridge>,
    /// WI-93: shared PTY registry handle (background terminal panel).
    pub(crate) terminal_service: std::sync::Arc<crate::terminal::TerminalService>,
    /// Shared browser session registry (kill_all at shutdown; desktop panel
    /// parity later).
    pub(crate) browser_service: std::sync::Arc<starriver_browser::BrowserService>,
    /// WI-103: per-agent routes switched through `set_agent_active_model`
    /// while a turn may still be running. The execute path clears an
    /// agent's entry once it seeded the run from the profile (the two then
    /// agree), and the `agent/request` rewrite listener registered per
    /// session applies a pending entry to every subsequent LLM call.
    desired_routes:
        Arc<std::sync::Mutex<std::collections::HashMap<String, (String, String, Option<u64>)>>>,
}

impl PluginKernel {
    pub fn new(
        ctx: cordis_rslite::Context,
        bus: cordis_rslite::EventBus,
        persistence: Arc<starriver_session_persistence::SqliteSessionPersistence>,
    ) -> Self {
        Self::with_scopes(ctx, bus, persistence, Arc::new(AgentScopeRegistry::new()))
    }

    pub fn with_scopes(
        ctx: cordis_rslite::Context,
        bus: cordis_rslite::EventBus,
        persistence: Arc<starriver_session_persistence::SqliteSessionPersistence>,
        scopes: Arc<AgentScopeRegistry>,
    ) -> Self {
        Self::with_services_and_web_overrides(
            ctx,
            bus,
            persistence,
            scopes,
            WebToolOverrides::default(),
        )
    }

    /// Test hook: like [`Self::with_scopes`], but swaps the registered web
    /// tool definitions for local-mock e2e.
    pub fn with_services_and_web_overrides(
        ctx: cordis_rslite::Context,
        bus: cordis_rslite::EventBus,
        persistence: Arc<starriver_session_persistence::SqliteSessionPersistence>,
        scopes: Arc<AgentScopeRegistry>,
        overrides: WebToolOverrides,
    ) -> Self {
        let hooks = Arc::new(crate::hooks::HookEngine::defaults());
        Self::with_services_web_overrides_and_hooks(ctx, bus, persistence, scopes, overrides, hooks)
    }

    /// Test hook: like [`Self::with_services_and_web_overrides`], but with an
    /// explicit hooks engine (tests point it at a temp directory).
    pub fn with_services_web_overrides_and_hooks(
        ctx: cordis_rslite::Context,
        bus: cordis_rslite::EventBus,
        persistence: Arc<starriver_session_persistence::SqliteSessionPersistence>,
        scopes: Arc<AgentScopeRegistry>,
        overrides: WebToolOverrides,
        hooks: Arc<crate::hooks::HookEngine>,
    ) -> Self {
        let sessions: Arc<dyn starriver_agent_loop::services::SessionStoreView> = Arc::new(
            starriver_session::InMemorySessionStore::new(bus.clone(), ctx.clone()),
        );
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::with_scopes(
            // Share the loop bus/ctx so `tools/pre-execute` listeners and the
            // `approval` service (registered on the loop ctx) stay reachable
            // from the tool runtime's own waterfall dispatch.
            bus.clone(),
            ctx.clone(),
            Arc::clone(&scopes),
        ));
        let tools_view: Arc<dyn starriver_agent_loop::services::ToolRuntimeView> =
            Arc::clone(&tools) as Arc<dyn starriver_agent_loop::services::ToolRuntimeView>;
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(
            cordis_rslite::EventBus::new(),
        ));
        let llm_view: Arc<dyn starriver_agent_loop::services::LlmRuntimeView> =
            Arc::clone(&llm) as Arc<dyn starriver_agent_loop::services::LlmRuntimeView>;
        // Shared system-prompt registry with the built-in residents. All
        // registrations land on the untagged global layer so every session
        // sees them; persona/review swap per host flow via the slots below.
        let system_prompts = Arc::new(SystemPromptRegistry::new());
        register_settings_residents(&system_prompts, &ctx);
        let skills_cell: std::sync::Arc<
            std::sync::Mutex<Option<Arc<starriver_skills::profile::skill_manager::SkillManager>>>,
        > = Arc::new(std::sync::Mutex::new(None));
        let skills_reader = Arc::clone(&skills_cell);
        let _skills_handle = system_prompts
            .section(
                &ctx,
                SectionEntry {
                    name: SKILLS_SECTION.into(),
                    order: 60,
                    complete: false,
                    text: SectionText::Dynamic(Arc::new(move |_| {
                        let guard = skills_reader.lock().unwrap();
                        match guard.as_ref() {
                            Some(manager) => always_on_skills_section(manager),
                            None => String::new(),
                        }
                    })),
                },
            )
            .ok();
        // Memory recall resident (WI-66 / BUG-1): the host writes the
        // recalled-facts section into `memory_slot` before each run; an empty
        // slot drops the section out of the assembly entirely.
        let memory_slot: Arc<std::sync::Mutex<String>> =
            Arc::new(std::sync::Mutex::new(String::new()));
        let memory_reader = Arc::clone(&memory_slot);
        // Feedback resident (WI-48 feedback slice): recent 👍/👎 records the
        // UI files through `feedback.record`; an empty list drops the section.
        let feedback_slot: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let feedback_reader = Arc::clone(&feedback_slot);
        let _feedback_handle = system_prompts
            .section(
                &ctx,
                SectionEntry {
                    name: "feedback".into(),
                    order: 75,
                    complete: false,
                    text: SectionText::Dynamic(Arc::new(move |_| {
                        let lines = feedback_reader.lock().unwrap();
                        if lines.is_empty() {
                            String::new()
                        } else {
                            format!(
                                "The user has given explicit feedback on earlier replies.\n\
                                 Apply it from now on: {}",
                                lines.join("; ")
                            )
                        }
                    })),
                },
            )
            .ok();
        let _memory_handle = system_prompts
            .section(
                &ctx,
                SectionEntry {
                    name: "memory".into(),
                    order: 70,
                    complete: false,
                    text: SectionText::Dynamic(Arc::new(move |_| {
                        memory_reader.lock().unwrap().clone()
                    })),
                },
            )
            .ok();
        // Per-run task skills resident (BUG-6): the host writes the selected
        // skills' bodies here before a run (scheduled tasks' `skills` list,
        // interactive slash dispatch); an empty slot drops the section — the
        // same contract as the memory resident.
        let skill_injection_slot: Arc<std::sync::Mutex<String>> =
            Arc::new(std::sync::Mutex::new(String::new()));
        let skill_injection_reader = Arc::clone(&skill_injection_slot);
        let _task_skills_handle = system_prompts
            .section(
                &ctx,
                SectionEntry {
                    name: "task-skills".into(),
                    order: 65,
                    complete: false,
                    text: SectionText::Dynamic(Arc::new(move |_| {
                        skill_injection_reader.lock().unwrap().clone()
                    })),
                },
            )
            .ok();
        // Unattended-run contract resident: the host writes the "no user is
        // present" section before a scheduled run and clears it afterwards;
        // an empty slot drops the section (interactive runs unaffected) —
        // same contract as the task-skills resident above.
        let unattended_slot: Arc<std::sync::Mutex<String>> =
            Arc::new(std::sync::Mutex::new(String::new()));
        let unattended_reader = Arc::clone(&unattended_slot);
        let _unattended_handle = system_prompts
            .section(
                &ctx,
                SectionEntry {
                    name: "run:unattended".into(),
                    order: 66,
                    complete: false,
                    text: SectionText::Dynamic(Arc::new(move |_| {
                        unattended_reader.lock().unwrap().clone()
                    })),
                },
            )
            .ok();
        // Plan fold (plan-mode parity): the `plan:policy` guidance
        // section at first-party order 500. It folds the session log, so it
        // appears exactly when the last `plan/mode` flip is active — the
        // pre-step listener lands pending flips before assembly, so each
        // request sees the state it will run under; inactive → empty → the
        // section drops out entirely.
        let _plan_policy_handle = system_prompts
            .section(
                &ctx,
                SectionEntry {
                    name: "plan:policy".into(),
                    order: 500,
                    complete: false,
                    text: SectionText::Dynamic(Arc::new(|render| {
                        if crate::plan_mode::effective_plan_mode(render.session) {
                            crate::plan_mode::PLAN_POLICY_SECTION.to_string()
                        } else {
                            // WI-178: standard mode gets the same plan-review
                            // loop — a compact hint so the model knows
                            // `exit_plan_mode` works here too.
                            crate::plan_mode::STANDARD_PLAN_HINT.to_string()
                        }
                    })),
                },
            )
            .ok();
        // SP2: cwd left the history snapshot — it now lives only in the
        // loop-registered `{{cwd}}` SYSTEM variable (frozen session header),
        // per prompt-parity D1. No host-side contribution is needed.
        let registry = Arc::new(starriver_agent_loop::registry::InMemoryAgentRegistry::new());
        let settings = crate::settings::load_settings();
        let mut preset_registry = HashMap::new();
        preset_registry.insert("coding".to_string(), coding_preset(&settings));
        let loop_service = Arc::new(starriver_agent_loop::agent_loop::AgentLoop::new(
            ctx.clone(),
            starriver_agent_loop::agent_loop::AgentLoopConfig::default(),
            Arc::clone(&sessions),
            tools_view,
            llm_view,
            Arc::clone(&registry) as Arc<dyn starriver_kernel::agent::AgentRegistry>,
            Some(Arc::clone(&persistence)
                as Arc<dyn starriver_agent_loop::services::PersistenceView>),
            preset_registry.clone(),
            Arc::clone(&scopes),
            Arc::clone(&system_prompts),
        ));
        let _factory_handle = registry
            .set_factory(Arc::clone(&loop_service) as Arc<dyn AgentFactory>)
            .unwrap();
        // Approvals wait indefinitely (dsh/codex parity): a request settles
        // only on an answer or a dropped client channel — never a clock.
        let approval_bridge = Arc::new(crate::approval_bridge::ApprovalBridge::new(Arc::clone(
            &registry,
        )
            as Arc<dyn starriver_kernel::agent::AgentRegistry>));
        let _bridge_handle = approval_bridge.register(&bus);
        // WI-67 (user-questions slice): agent-initiated questions surface as
        // UiEvents through the same session-event chain as approvals.
        let question_bridge = Arc::new(crate::question_bridge::QuestionBridge::new(Arc::clone(
            &registry,
        )
            as Arc<dyn starriver_kernel::agent::AgentRegistry>));
        // Plan fold (plan-mode parity): the execution-mode lock is shared
        // with the plan controller so the approved exit bundles the
        // workspace-write release; the review bridge answers exit_plan_mode.
        let execution_mode: Arc<std::sync::Mutex<Option<crate::execution_mode::ExecutionMode>>> =
            Arc::new(std::sync::Mutex::new(None));
        let plan_controller =
            crate::plan_mode::PlanModeController::new(Arc::clone(&execution_mode));
        let plan_review_bridge = Arc::new(crate::plan_review_bridge::PlanReviewBridge::new(
            Arc::clone(&registry) as Arc<dyn starriver_kernel::agent::AgentRegistry>,
        ));
        // Permission preset: every fresh session starts workspace-write + ask.
        let preset: Arc<std::sync::Mutex<crate::preset::Preset>> =
            Arc::new(std::sync::Mutex::new(crate::preset::Preset::default()));
        let _preset_pinner = crate::preset::register_preset_pinner_with(&bus, Arc::clone(&preset));
        // WI-115 hooks. Registered BEFORE the approval listener on purpose:
        // the waterfall chain wraps listeners around the innermost callback in
        // registration order, so the FIRST-registered listener runs FIRST —
        // a hook verdict lands before the user would be prompted. An exit-2
        // Pre hook denies outright (never reaches approval); everything else
        // falls through to the approval/preset chain via next(). Per book
        // ch12 §12.5 hooks are an extension point, not a security gate:
        // spawn errors and timeouts never block here (the engine warns and
        // allows).
        let hooks_pre = Arc::clone(&hooks);
        let _hooks_pre_trigger = bus.on_waterfall::<ToolsPreExecute, _>(move |event, next| {
            let hooks_pre = Arc::clone(&hooks_pre);
            Box::pin(async move {
                let params =
                    starriver_kernel::json::JsonValue::from_value(&event.execution.arguments)
                        .ok()
                        .and_then(|v| v.to_json_string().ok());
                let working_dir = event
                    .execution
                    .cwd
                    .clone()
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let verdict = hooks_pre
                    .trigger_context(
                        &crate::hooks::HookContext {
                            event: "PreToolUse",
                            tool: &event.execution.name,
                            session: event.execution.agent_id.as_deref(),
                            workspace: event.execution.cwd.as_deref(),
                            params_json: params.as_deref(),
                            result_json: None,
                        },
                        &working_dir,
                    )
                    .await;
                if !verdict.allowed {
                    return Ok(PreToolDecision::Deny {
                        reason: verdict
                            .output
                            .unwrap_or_else(|| "denied by PreToolUse hook".into()),
                    });
                }
                next().await
            })
        });
        let tools_trigger = Arc::clone(&tools);
        let _trigger = bus.on_waterfall::<ToolsPreExecute, _>({
            let tools_trigger = Arc::clone(&tools_trigger);
            move |event, next| {
                let tools_trigger = Arc::clone(&tools_trigger);
                Box::pin(async move {
                    let name = event.execution.name.clone();
                    // Danger-full-access: nothing asks; the sandbox does not
                    // constrain and approval policy never would reject.
                    if event.execution.sandbox_mode
                        == Some(starriver_kernel::sandbox::SandboxMode::DangerFullAccess)
                    {
                        return next().await;
                    }
                    // Read-only sandbox: file-writing tools are refused before
                    // approval (asking cannot change the sandbox outcome).
                    if event.execution.sandbox_mode
                        == Some(starriver_kernel::sandbox::SandboxMode::ReadOnly)
                    {
                        // WI-12: bash gets a per-command verdict — pure reads
                        // (ls/cat/grep/git status …) pass, anything with write
                        // potential is refused. Fail-closed: unparseable or
                        // missing commands count as writes.
                        if name == "bash" {
                            let command = starriver_kernel::json::JsonValue::from_value(
                                &event.execution.arguments,
                            )
                            .ok()
                            .and_then(|args| {
                                args.get("command").and_then(|c| c.as_str()).map(String::from)
                            });
                            let readonly = command
                                .as_deref()
                                .map(crate::tools::bash_readonly::bash_command_is_readonly)
                                .unwrap_or(false);
                            if !readonly {
                                return Ok(PreToolDecision::Deny {
                                    reason: "read-only sandbox mode refuses bash commands with write potential"
                                        .into(),
                                });
                            }
                            // A verified pure read is fully contained by the
                            // read-only sandbox — approval cannot improve the
                            // outcome, so plan-mode exploration runs without
                            // prompts (pwd && ls -la must not ask).
                            return next().await;
                        } else if tools_trigger.deny_in_readonly(&name) {
                            return Ok(PreToolDecision::Deny {
                                reason: "read-only sandbox mode refuses file writes".into(),
                            });
                        }
                    }
                    // Permission-preset model: requires_approval tools always
                    // ask; approval policy (never) rejects inside the service.
                    if tools_trigger.requires_approval(&name) {
                        Ok(PreToolDecision::Ask {
                            reason: Some(format!("tool \"{name}\" requires approval")),
                        })
                    } else {
                        next().await
                    }
                })
            }
        });
        // Post hooks: observation only. The verdict is discarded and the
        // original result always flows on — no exit code can alter it.
        let hooks_post = Arc::clone(&hooks);
        let _hooks_post_trigger = bus.on_waterfall::<ToolsPostExecute, _>(move |event, next| {
            let hooks_post = Arc::clone(&hooks_post);
            Box::pin(async move {
                let result_json = match &event.result {
                    ToolExecutionResult::Success { value, .. } => {
                        let value = starriver_kernel::json::JsonValue::from_value(value)
                            .ok()
                            .and_then(|v| v.to_json_string().ok())
                            .unwrap_or_else(|| "null".into());
                        format!(r#"{{"ok":true,"value":{value}}}"#)
                    }
                    ToolExecutionResult::Failure { error, .. } => {
                        let message = serde_json::json!(error.message.clone()).to_string();
                        format!(r#"{{"ok":false,"error":{message}}}"#)
                    }
                };
                let working_dir = event
                    .execution
                    .cwd
                    .clone()
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let _ = hooks_post
                    .trigger_context(
                        &crate::hooks::HookContext {
                            event: "PostToolUse",
                            tool: &event.execution.name,
                            session: event.execution.agent_id.as_deref(),
                            workspace: event.execution.cwd.as_deref(),
                            params_json: None,
                            result_json: Some(&result_json),
                        },
                        &working_dir,
                    )
                    .await;
                next().await
            })
        });
        // LSP registry: builtin servers, overridden/extended by
        // settings.yaml (`lsp.servers` — same id replaces, others append).
        let mut lsp_specs = crate::lsp::builtin_servers();
        for custom in &settings.lsp.servers {
            if let Some(existing) = lsp_specs.iter_mut().find(|s| s.id == custom.id) {
                *existing = crate::lsp::ServerSpec {
                    id: custom.id.clone(),
                    extensions: custom.extensions.clone(),
                    command: custom.command.clone(),
                    args: custom.args.clone(),
                    root_markers: custom.root_markers.clone(),
                    language_id: custom
                        .language_id
                        .clone()
                        .unwrap_or_else(|| custom.id.clone()),
                    install_hint: format!(
                        "configure `{}` correctly in settings.yaml",
                        custom.command
                    ),
                };
            } else {
                lsp_specs.push(crate::lsp::ServerSpec {
                    id: custom.id.clone(),
                    extensions: custom.extensions.clone(),
                    command: custom.command.clone(),
                    args: custom.args.clone(),
                    root_markers: custom.root_markers.clone(),
                    language_id: custom
                        .language_id
                        .clone()
                        .unwrap_or_else(|| custom.id.clone()),
                    install_hint: format!(
                        "configure `{}` correctly in settings.yaml",
                        custom.command
                    ),
                });
            }
        }
        let lsp_manager = Arc::new(crate::lsp::LspManager::with_specs(lsp_specs));
        let web_fetch_def = overrides
            .web_fetch
            .unwrap_or_else(crate::tools::web_fetch::web_fetch_definition);
        let web_search_def = overrides
            .web_search
            .unwrap_or_else(crate::tools::web_search::web_search_definition);
        // Rewritten core tools on the new seam (batch 1) plus the batch 2 set.
        // BUG-101: bash timeout limits are settings-tunable
        // (agent.bash_timeout_secs default 120, agent.bash_timeout_max_secs
        // hard cap 1800).
        let bash_limits = crate::tools::bash::BashTimeoutLimits {
            default_secs: settings
                .agent
                .bash_timeout_secs
                .unwrap_or(crate::tools::bash::DEFAULT_BASH_TIMEOUT_SECS),
            max_secs: settings
                .agent
                .bash_timeout_max_secs
                .unwrap_or(crate::tools::bash::DEFAULT_BASH_TIMEOUT_MAX_SECS),
        };
        // BUG-157: settings-provided tool environment (agent.tool_env),
        // expanded against the daemon process env once at registration —
        // e.g. `{ "PATH": "$HOME/.cargo/bin:$PATH" }` makes a headless
        // daemon's bash tool see the user toolchain without launch-time
        // env plumbing.
        let bash_tool_env = settings
            .agent
            .tool_env
            .as_ref()
            .map(crate::tools::bash::expand_tool_env);
        // browser parity: first-party CDP browser sessions shared across
        // calls (settings-tunable; an invalid viewport warns and keeps the
        // default rather than failing boot).
        let browser_config = {
            let mut config = starriver_browser::BrowserServiceConfig::default();
            config.browser_path = settings.browser.browser_path.clone();
            config.headless = settings.browser.headless.unwrap_or(config.headless);
            if let Some(spec) = settings.browser.viewport.as_deref() {
                match starriver_browser::axtree::parse_viewport(Some(spec)) {
                    Ok(viewport) => config.viewport = viewport,
                    Err(e) => {
                        tracing::warn!(viewport = %spec, error = %e, "invalid browser.viewport, keeping default")
                    }
                }
            }
            config.idle_timeout_secs = settings
                .browser
                .idle_timeout_secs
                .unwrap_or(config.idle_timeout_secs);
            config.max_sessions = settings.browser.max_sessions.unwrap_or(config.max_sessions);
            config
        };
        let browser_service = Arc::new(starriver_browser::BrowserService::new(browser_config));
        let browser_service_handle = Arc::clone(&browser_service);
        let browser_defs = crate::tools::browser::browser_definitions(
            Arc::clone(&browser_service),
            crate::attachments::open_attachment_store(),
        );
        for mut definition in [
            crate::tools::bash::bash_definition_with_env(bash_limits, bash_tool_env),
            // BUG-155: tool-level skill loading — prose trigger lines
            // measured 0 loads in ~40 harness2 rollouts. The executors
            // read the same cell the always-on resident reads; before the
            // host attaches they fail with SKILLS_UNAVAILABLE.
            crate::tools::skill::skill_list_definition(Arc::clone(&skills_cell)),
            crate::tools::skill::skill_load_definition(Arc::clone(&skills_cell)),
            crate::tools::read::read_definition(),
            crate::tools::write::write_definition(),
            crate::tools::edit::edit_definition(),
            crate::tools::git::git_definition(),
            crate::tools::grep::grep_definition(),
            crate::tools::glob::glob_definition(),
            web_fetch_def,
            web_search_def,
            crate::tools::lsp::lsp_definition(Arc::clone(&lsp_manager)),
            crate::tools::patch::patch_definition(),
            crate::tools::todo::todo_write_definition(),
            crate::tools::goal::goal_write_definition(),
            crate::tools::plan::plan_write_definition(),
            crate::tools::code_runtime::python_exec_definition(),
            crate::tools::code_runtime::e2b_run_definition(),
        ]
        .into_iter()
        .chain(browser_defs)
        {
            // spill parity: oversized outputs are persisted and replaced
            // by a summary for every tool that has no custom finalizer.
            // WI-10: the threshold is settings-tunable (agent.spill_threshold_bytes).
            if definition.finalize_content.is_none() {
                definition.finalize_content =
                    Some(crate::tools::spill::spill_finalizer_with_threshold(
                        settings
                            .agent
                            .spill_threshold_bytes
                            .unwrap_or(crate::tools::spill::SPILL_THRESHOLD_BYTES),
                    ));
            }
            let _ = tools.register(definition);
        }
        // tool-guide parity: every first-party tool family carries a
        // `<tool>:guide` division-of-labor section in the reserved 100..=199
        // band (search with grep — not shell grep; inspect with read — not
        // cat; find with glob; edit over wholesale write; PTY hygiene;
        // untrusted web data). Without these the model only sees bare tool
        // schemas and habitually batches file inspection into shell commands.
        // Static text, always rendered — the same untagged global layer as
        // the residents above.
        for (tool, order, text) in crate::tools::tool_guide_sections() {
            debug_assert!(
                starriver_kernel::system_prompt::TOOL_GUIDE_ORDER_MIN <= order
                    && order <= starriver_kernel::system_prompt::TOOL_GUIDE_ORDER_MAX,
                "{tool}:guide order {order} outside the reserved band"
            );
            let _ = system_prompts.section(
                &ctx,
                SectionEntry {
                    name: format!("{tool}:guide"),
                    order,
                    complete: false,
                    text: SectionText::Static(text.into()),
                },
            );
        }
        // WI-1: reload_image lets the model pull a historical image back
        // into its context (turn-scoped hot set in the loop's projection).
        // Settings-gated; on by default. Same store root as the runtime's
        // instance (content-addressed, stateless).
        if settings.agent.enable_reload_image != Some(false) {
            let mut definition = crate::tools::reload_image::reload_image_definition(
                crate::attachments::open_attachment_store(),
            );
            if definition.finalize_content.is_none() {
                definition.finalize_content =
                    Some(crate::tools::spill::spill_finalizer_with_threshold(
                        settings
                            .agent
                            .spill_threshold_bytes
                            .unwrap_or(crate::tools::spill::SPILL_THRESHOLD_BYTES),
                    ));
            }
            let _ = tools.register(definition);
        }
        // WI-6: session_reference lets the model quote another conversation's
        // surface on demand. Settings-gated; on by default.
        if settings.agent.enable_session_reference != Some(false) {
            let mut definition = crate::tools::session_reference::session_reference_definition(
                Arc::clone(&persistence),
            );
            if definition.finalize_content.is_none() {
                definition.finalize_content =
                    Some(crate::tools::spill::spill_finalizer_with_threshold(
                        settings
                            .agent
                            .spill_threshold_bytes
                            .unwrap_or(crate::tools::spill::SPILL_THRESHOLD_BYTES),
                    ));
            }
            let _ = tools.register(definition);
        }
        // Plan fold (plan-mode parity): `exit_plan_mode` stays registered
        // in every catalog — entering or leaving plan mode changes only the
        // prompt section, never the tool catalog; execution outside plan
        // mode fails (the reviewer closure owns that verdict).
        {
            let review_bridge = Arc::clone(&plan_review_bridge);
            let review_controller = Arc::clone(&plan_controller);
            let review_registry =
                Arc::clone(&registry) as Arc<dyn starriver_kernel::agent::AgentRegistry>;
            let plan_reviewer: crate::tools::exit_plan_mode::PlanReviewer = Arc::new(
                move |session_id: String, call_id: Option<String>, plan: String| {
                    let bridge = Arc::clone(&review_bridge);
                    let controller = Arc::clone(&review_controller);
                    let registry = Arc::clone(&review_registry);
                    Box::pin(async move {
                        let Some(agent) =
                            registry.get(&starriver_kernel::session::SessionId(session_id.clone()))
                        else {
                            return Err("unavailable".into());
                        };
                        // WI-178: the review bridge answers in EVERY mode —
                        // plan mode keeps its exit semantics, and standard
                        // mode gets the same plan→review→code loop (the
                        // desktop Plan panel is mode-agnostic). No
                        // not-in-plan-mode short circuit any more.
                        match bridge
                            .request(
                                &session_id,
                                call_id.as_deref(),
                                &plan,
                                crate::plan_review_bridge::DEFAULT_PLAN_REVIEW_TIMEOUT,
                            )
                            .await
                        {
                            Ok(crate::plan_review_bridge::PlanDecision::Approved) => {
 // Silent pending exit: the
                                // pre-step listener lands the flip — guidance
                                // and the read-only pairing hold for the rest
                                // of the current tool batch.
                                controller.select(&agent, false);
                                Ok(crate::tools::exit_plan_mode::PlanReviewVerdict::Approved)
                            }
                            Ok(crate::plan_review_bridge::PlanDecision::Rejected { feedback }) => {
                                Ok(crate::tools::exit_plan_mode::PlanReviewVerdict::Rejected {
                                    feedback,
                                })
                            }
                            Err(crate::plan_review_bridge::ReviewError::Timeout) => Err(
                                "计划评审等待超时（5 分钟未响应），已暂停执行——重新提交计划或回复消息可继续"
                                    .to_string(),
                            ),
                            Err(error) => Err(error.as_str().to_string()),
                        }
                    })
                },
            );
            let _ = tools.register(crate::tools::exit_plan_mode::exit_plan_mode_definition(
                plan_reviewer,
            ));
        }
        // Backlog #6 presentation seam, first resident: todo_write attaches
        // structured counts so the UI card can narrow from `meta` instead of
        // re-parsing rendered text. Pure over args; soft-fails (None) on any
        // unexpected shape.
        tools.register_presentation_meta(
            "todo_write",
            Arc::new(|args, _value| {
                let args = starriver_kernel::json::JsonValue::from_value(args).ok()?;
                let items = args.get("items")?.as_array()?;
                let total = items.len();
                let completed = items
                    .iter()
                    .filter(|item| {
                        item.get("checked") == Some(&starriver_kernel::json::JsonValue::Bool(true))
                            || item.get("status").and_then(|s| s.as_str()) == Some("completed")
                    })
                    .count();
                Some(serde_json::json!({
                    "kind": "todo",
                    "total": total,
                    "completed": completed,
                }))
            }),
        );
        // terminal parity: persistent PTY sessions shared across calls.
        let terminal_service = Arc::new(crate::terminal::TerminalService::new());
        let terminal_service_handle = Arc::clone(&terminal_service);
        for definition in
            crate::tools::terminal::terminal_definitions(Arc::clone(&terminal_service))
        {
            let _ = tools.register(definition);
        }
        // PTC transport: the `run_code` definition plus its ptc-only guard.
        // Presentation stays inert until an execution mode flips the flag via
        // `apply_execution_mode` — until then schemas list every tool and
        // native calls pass the guard untouched.
        let _run_code_handle = starriver_tools::ptc::register_run_code(&tools);
        // LSP diagnostics auto-feedback: after a successful file write / edit /
        // patch, refresh the file with its language server (when one is
        // registered for the extension) and feed fresh diagnostics back as
        // deferred context — the model learns about type errors on the step
        // that caused them instead of on a later read.
        let lsp_for_feedback = Arc::clone(&lsp_manager);
        let _lsp_feedback = bus.on_waterfall::<ToolsPostExecute, _>(move |event, next| {
            let lsp_for_feedback = Arc::clone(&lsp_for_feedback);
            Box::pin(async move {
                let tool_name = event.execution.name.clone();
                let writes_file = matches!(tool_name.as_str(), "write" | "edit" | "patch");
                let path_arg = if writes_file {
                    crate::tools::common::json_args(&event.execution.arguments)
                        .ok()
                        .and_then(|json| {
                            json.get("path")
                                .and_then(|p| p.as_str())
                                .map(|p| p.to_string())
                        })
                } else {
                    None
                };
                let cwd = event.execution.cwd.clone();
                let call_id = event.execution.call_id.0.clone();
                let decision = next().await?;
                let mut decision = decision;
                let Some(path_arg) = path_arg else {
                    return Ok(decision);
                };
                let PostToolDecision::Accept {
                    additional_contexts,
                    ..
                } = &mut decision
                else {
                    return Ok(decision);
                };
                let path = crate::tools::common::resolve_path(cwd.as_deref(), &path_arg);
                let Ok((client, _spec)) = lsp_for_feedback.session_for_file(&path).await else {
                    return Ok(decision);
                };
                let Ok(text) = tokio::fs::read_to_string(&path).await else {
                    return Ok(decision);
                };
                let uri = crate::lsp::file_uri(&path).as_str().to_string();
                if client.open_or_update(&uri, text).await.is_err() {
                    return Ok(decision);
                }
                let diags = client
                    .diagnostics(&uri, std::time::Duration::from_millis(900))
                    .await;
                if diags.is_empty() {
                    return Ok(decision);
                }
                let label = |severity: Option<lsp_types::DiagnosticSeverity>| match severity {
                    Some(lsp_types::DiagnosticSeverity::ERROR) => "error",
                    Some(lsp_types::DiagnosticSeverity::WARNING) => "warning",
                    Some(lsp_types::DiagnosticSeverity::INFORMATION) => "info",
                    _ => "hint",
                };
                let lines: Vec<String> = diags
                    .iter()
                    .take(5)
                    .map(|d| {
                        format!(
                            "- [{}] {} (line {})",
                            label(d.severity),
                            d.message.replace('\n', " "),
                            d.range.start.line + 1
                        )
                    })
                    .collect();
                let summary = format!(
                    "LSP diagnostics for {} after this {}: {} item(s):\n{}{}",
                    path.display(),
                    tool_name,
                    diags.len(),
                    lines.join("\n"),
                    if diags.len() > 5 {
                        "\n… (truncated)".to_string()
                    } else {
                        String::new()
                    },
                );
                additional_contexts.push(Message {
                    id: MessageId(format!("lsp-diag-{call_id}")),
                    role: MessageRole::User,
                    content: vec![ContentBlock::Text(summary)],
                    source: MessageSource::User,
                });
                Ok(decision)
            })
        });
        Self {
            ctx,
            registry,
            terminal_service: terminal_service_handle,
            browser_service: browser_service_handle,
            desired_routes: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            loop_service,
            persistence,
            sessions,
            tools,
            mcp: Arc::new(std::sync::Mutex::new(McpToolRegistry::default())),
            llm,
            preset_registry,
            scopes,
            system_prompts,
            skills_cell,
            persona_slot: std::sync::Mutex::new(None),
            memory_slot,
            skill_injection_slot,
            unattended_slot,
            feedback_slot,
            review_slot: std::sync::Mutex::new(None),
            review_whitelist: std::sync::Mutex::new(None),
            approval_bridge,
            question_bridge,
            preset,
            preset_override: std::sync::atomic::AtomicBool::new(false),
            execution_mode,
            plan_controller,
            plan_review_bridge,
        }
    }

    /// Access the shared registry (tests and host flows register scoped or
    /// extra global residents here).
    pub fn system_prompts(&self) -> Arc<SystemPromptRegistry> {
        Arc::clone(&self.system_prompts)
    }

    /// Plan fold: the pending-flip controller (host UI flows select state;
    /// the per-agent pre-step listener is registered per publication).
    pub fn plan_controller(&self) -> Arc<crate::plan_mode::PlanModeController> {
        Arc::clone(&self.plan_controller)
    }

    /// The active execution profile, if set (Plan strict-lock checks on the
    /// respond_plan_review path read this).
    pub fn execution_mode(&self) -> Option<crate::execution_mode::ExecutionMode> {
        *self.execution_mode.lock().unwrap()
    }

    /// Upsert one agent-preset composition into the loop's registry so
    /// create- and resume-path resolution both see it (WI-5: the host builds
    /// the options from AGENT.md `preset:` blocks on every run).
    pub fn upsert_agent_preset(&self, id: String, options: starriver_kernel::agent::AgentOptions) {
        self.loop_service
            .preset_registry_handle()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, options);
    }

    /// Attach the always-on skill source read by the `skills:always-on`
    /// resident on every assembly. Swappable at any time — the resident is a
    /// dynamic provider.
    pub fn attach_skill_manager(
        &self,
        manager: Arc<starriver_skills::profile::skill_manager::SkillManager>,
    ) {
        *self.skills_cell.lock().unwrap() = Some(manager);
    }

    /// Replace or clear the persona resident (`deployment:persona`, order 0)
    /// on the global layer — the switch-agent flow. Single-active-agent
    /// semantics for now: one persona slot, replaced wholesale.
    pub async fn set_persona(&self, body: Option<String>) -> anyhow::Result<()> {
        // Take the old handle outside await points (clippy await-holding-lock).
        let stale = self.persona_slot.lock().unwrap().take();
        if let Some(old) = stale {
            old.dispose_async().await;
        }
        if let Some(body) = body {
            let handle = self
                .system_prompts
                .section(
                    &self.ctx,
                    SectionEntry {
                        name: PERSONA_SECTION.into(),
                        order: 0,
                        complete: false,
                        text: SectionText::Static(body),
                    },
                )
                .map_err(|e| anyhow::anyhow!("persona register: {e}"))?;
            *self.persona_slot.lock().unwrap() = Some(handle);
        }
        Ok(())
    }

    /// Point the memory resident at `text` for the runs that follow
    /// (`None` clears — e.g. agents without a memory profile, scheduled
    /// runs). The resident itself stays registered; an empty slot renders
    /// as nothing.
    pub fn set_memory_injection(&self, text: Option<String>) {
        *self.memory_slot.lock().unwrap() = text.unwrap_or_default();
    }

    /// Point the `task-skills` resident at `text` for the runs that follow
    /// (`None` clears — runs without task-bound skills must not inherit a
    /// stale section). Mirrors [`Self::set_memory_injection`].
    pub fn set_skill_injection(&self, text: Option<String>) {
        *self.skill_injection_slot.lock().unwrap() = text.unwrap_or_default();
    }

    /// Point the `run:unattended` resident at `text` for the runs that follow
    /// (`None` clears — interactive runs must never inherit the unattended
    /// contract). Mirrors [`Self::set_skill_injection`].
    pub fn set_unattended_injection(&self, text: Option<String>) {
        *self.unattended_slot.lock().unwrap() = text.unwrap_or_default();
    }

    /// Test observation hook: the current `run:unattended` resident text.
    #[doc(hidden)]
    pub fn debug_unattended_injection(&self) -> String {
        self.unattended_slot.lock().unwrap().clone()
    }

    /// Test observation hook: the current `task-skills` resident text.
    #[doc(hidden)]
    pub fn debug_skill_injection(&self) -> String {
        self.skill_injection_slot.lock().unwrap().clone()
    }

    /// Activate review mode: registers the complete:true resident that
    /// replaces the whole system prompt while active. An optional tool
    /// whitelist replaces the visible-tool set for sessions started while
    /// review is active (legacy `tool_whitelist`, now actually enforced).
    pub async fn enter_review_mode(
        &self,
        prompt: String,
        tool_whitelist: Option<Vec<String>>,
    ) -> anyhow::Result<()> {
        let stale = self.review_slot.lock().unwrap().take();
        if let Some(old) = stale {
            old.dispose_async().await;
        }
        let handle = self
            .system_prompts
            .section(
                &self.ctx,
                SectionEntry {
                    name: REVIEW_SECTION.into(),
                    order: 80,
                    complete: true,
                    text: SectionText::Static(prompt),
                },
            )
            .map_err(|e| anyhow::anyhow!("review register: {e}"))?;
        *self.review_slot.lock().unwrap() = Some(handle);
        *self.review_whitelist.lock().unwrap() = tool_whitelist;
        Ok(())
    }

    /// Deactivate review mode (disposing restores identity/persona/skills and
    /// releases the tool whitelist).
    pub async fn exit_review_mode(&self) {
        let stale = self.review_slot.lock().unwrap().take();
        if let Some(old) = stale {
            old.dispose_async().await;
        }
        *self.review_whitelist.lock().unwrap() = None;
    }

    /// Override the preset pinned into subsequently created sessions (tests
    /// and the UI preset selector use this). Marks the preset as explicitly
    /// overridden: the execution-mode preset refresh then leaves it alone.
    pub fn set_preset(&self, preset: crate::preset::Preset) {
        *self.preset.lock().unwrap() = preset;
        self.preset_override
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Observe the currently pinned preset (tests).
    #[cfg(test)]
    pub(crate) fn debug_preset(&self) -> crate::preset::Preset {
        self.preset.lock().unwrap().clone()
    }

    /// Push the current execution mode (host boot / `session.setExecutionMode`).
    /// Refreshes the pinned preset to the mode's defaults unless it was
    /// explicitly overridden via [`Self::set_preset`].
    pub fn apply_execution_mode(&self, mode: Option<crate::execution_mode::ExecutionMode>) {
        *self.execution_mode.lock().unwrap() = mode;
        // PTC rides the same push: the tool runtime flips its schema
        // projection and arms the ptc-only guard for this mode only. Set
        // before the preset-override early return — the flag flip is not a
        // preset concern.
        self.tools
            .set_ptc_active(mode == Some(crate::execution_mode::ExecutionMode::Ptc));
        if self
            .preset_override
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        use crate::preset::MANUAL_APPROVAL;
        use starriver_kernel::sandbox::SandboxMode;
        let (sandbox, approval) = match mode {
            // Plan planning phase: read-only + Manual (the execution-phase unlock is stamped by confirmPlan).
            Some(crate::execution_mode::ExecutionMode::Plan) => {
                (SandboxMode::ReadOnly, MANUAL_APPROVAL)
            }
            // Other modes (including None falling back to Standard) read the
            // profile table — the single source of truth, avoiding drift from
            // duplicating the defaults here and in execution_mode::profile.
            other => {
                let p = crate::execution_mode::profile(
                    other.unwrap_or(crate::execution_mode::ExecutionMode::Standard),
                );
                (p.preset_sandbox, p.preset_approval)
            }
        };
        *self.preset.lock().unwrap() = crate::preset::Preset { sandbox, approval };
    }
    /// WI-48 feedback slice: add one rendered feedback line to the dynamic
    /// resident slot (keep last 10 — guidance, not an archive).
    pub fn push_feedback_line(&self, line: String) {
        let mut slot = self.feedback_slot.lock().unwrap();
        slot.push(line);
        let len = slot.len();
        if len > 10 {
            slot.drain(..len - 10);
        }
    }

    pub fn registry(&self) -> Arc<starriver_agent_loop::registry::InMemoryAgentRegistry> {
        Arc::clone(&self.registry)
    }

    /// WI-103: remember a route switched through `set_agent_active_model`
    /// so a running turn adopts it on its next LLM call. The explicit
    /// per-provider max_tokens is captured here so the rewrite applies the
    /// new provider's configured cap too (BUG-88) instead of dragging the
    /// seeded provider's along.
    pub fn record_desired_route(&self, agent_name: &str, provider: &str, model: &str) {
        let settings = crate::settings::load_settings();
        let key = format!("{provider}/{model}");
        let max_tokens = provider_entry_for(&settings, &key)
            .and_then(|p| p.max_tokens)
            .or_else(|| provider_entry_for(&settings, model).and_then(|p| p.max_tokens));
        self.desired_routes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                agent_name.to_string(),
                (
                    provider.to_string(),
                    model.to_string(),
                    max_tokens.map(|v| v as u64),
                ),
            );
    }

    /// WI-103/BUG-88: drop an agent's pending entry only when the run just
    /// seeded the SAME route from the profile. A switch that landed between
    /// the profile read and this call (or one pending for another session of
    /// the agent) must survive — clearing it would silently lose the user's
    /// switch for the whole turn.
    pub fn clear_desired_route_if(
        &self,
        agent_name: &str,
        seeded_provider: Option<&str>,
        seeded_model: Option<&str>,
    ) {
        let mut routes = self
            .desired_routes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let matches_seed = routes.get(agent_name).is_some_and(|(p, m, _)| {
            Some(p.as_str()) == seeded_provider && Some(m.as_str()) == seeded_model
        });
        if matches_seed {
            routes.remove(agent_name);
        }
    }

    /// Register the per-session `agent/request` rewrite listener that lands
    /// a mid-run route switch on every subsequent LLM call (the turn keeps
    /// its context; only the route changes). Same registration shape as the
    /// plan controller's pre-step listener.
    pub fn register_route_rewrite(
        &self,
        agent: &Arc<dyn Agent>,
        agent_name: String,
    ) -> cordis_rslite::EffectHandle {
        let desired = Arc::clone(&self.desired_routes);
        let llm = Arc::clone(&self.llm);
        agent
            .ctx()
            .on_waterfall::<AgentRequest, _>(move |_event, next| {
                let desired = Arc::clone(&desired);
                let llm = Arc::clone(&llm);
                let agent_name = agent_name.clone();
                Box::pin(async move {
                    let mut config = next().await?;
                    if let Some((provider, model, max_tokens)) = desired
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&agent_name)
                        .cloned()
                    {
                        // BUG-88: only steer onto a route that actually
                        // registered — a switch whose engine activation
                        // failed (no adapter for the provider) must not
                        // kill a healthy running turn mid-flight; it takes
                        // effect at the next execute's seeding instead.
                        if llm.registered_providers().iter().any(|p| p == &provider) {
                            config.provider = provider;
                            config.model = model;
                            // The explicit per-provider cap follows the route
                            // (None = unset — don't drag the old cap along).
                            config.max_tokens = max_tokens;
                        }
                    }
                    Ok(config)
                })
            })
    }

    /// Tool runtime backing this kernel; consumed by host admin/agents tool
    /// registry accessors (concrete type so register/list stay reachable).
    pub fn tools(&self) -> &starriver_tools::InMemoryToolRuntime {
        self.tools.as_ref()
    }

    /// Register MCP tool definitions for one server on the kernel seam.
    /// Replaces any earlier registration for the same server; a failed
    /// registration rolls back the whole batch.
    pub async fn register_mcp_definitions(
        &self,
        server: &str,
        definitions: Vec<ToolDefinition>,
    ) -> anyhow::Result<Vec<String>> {
        self.unregister_mcp_definitions(server).await;
        let mut registered: Vec<(String, EffectHandle)> = Vec::new();
        for definition in definitions {
            let name = definition.name.clone();
            match self.tools.register(definition) {
                Ok(handle) => registered.push((name, handle)),
                Err(error) => {
                    for (_, handle) in registered.drain(..) {
                        handle.dispose_async().await;
                    }
                    return Err(anyhow::anyhow!(
                        "MCP tool registration failed for server {server}: {error}"
                    ));
                }
            }
        }
        let names = registered
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        self.mcp
            .lock()
            .unwrap()
            .by_server
            .insert(server.to_string(), registered);
        Ok(names)
    }

    /// Dispose every kernel-seam MCP registration for one server.
    pub async fn unregister_mcp_definitions(&self, server: &str) {
        let removed = self.mcp.lock().unwrap().by_server.remove(server);
        if let Some(entries) = removed {
            for (_, handle) in entries {
                handle.dispose_async().await;
            }
        }
    }

    fn mcp_tool_names(&self) -> Vec<String> {
        self.mcp.lock().unwrap().names()
    }

    /// Test-only hook: register a mock adapter into the llm runtime.
    pub fn set_llm_adapter(&self, adapter: Arc<dyn starriver_kernel::llm::LlmAdapter>) {
        let _handle = self.llm.register_adapter(adapter).unwrap();
    }

    /// The kernel LLM runtime (adapter registry + stream/chat seam), shared
    /// with host-side consumers such as title generation and memory
    /// `llm_select`.
    pub fn llm(&self) -> Arc<starriver_llm::InMemoryLlmRuntime> {
        self.llm.clone()
    }

    /// Register (or replace) one self-declaring protocol adapter. Providers
    /// the adapter declares are unregistered first so re-activating a model
    /// route replaces it; the registration itself is atomic, so a conflict
    /// with another plugin's route fails the whole registration.
    pub fn register_provider_adapter(&self, adapter: Arc<dyn starriver_kernel::llm::LlmAdapter>) {
        for provider in adapter.providers() {
            self.llm.unregister_provider(provider);
        }
        let _handle = self.llm.register_adapter(adapter);
    }

    /// Test observation hook: all registered provider ids.
    #[doc(hidden)]
    pub fn debug_provider_routes(&self) -> Vec<String> {
        self.llm.registered_providers()
    }

    /// The default provider for new agents: the first registered route, or
    /// `mock` when none (tests and unconfigured desktops).
    pub fn default_provider(&self) -> String {
        self.llm
            .registered_providers()
            .first()
            .cloned()
            .unwrap_or_else(|| "mock".into())
    }

    /// Execute a task on a fresh or resumed session. `session_id = None`
    /// creates a new session; `Some(id)` resumes it (NotFound falls back to
    /// create). Events are bridged to `UiEvent` JSON on `events`.
    pub async fn execute(
        &self,
        task: String,
        session_id: Option<String>,
        model: Option<String>,
        events: tokio::sync::broadcast::Sender<serde_json::Value>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<KernelExecution> {
        self.execute_with_meta(task, session_id, model, events, cancel, None)
            .await
    }

    /// Like [`Self::execute`], with host session metadata appended to the log
    /// before the first user message.
    pub async fn execute_with_meta(
        &self,
        task: String,
        session_id: Option<String>,
        model: Option<String>,
        events: tokio::sync::broadcast::Sender<serde_json::Value>,
        cancel: tokio_util::sync::CancellationToken,
        meta: Option<HostSessionMeta>,
    ) -> anyhow::Result<KernelExecution> {
        self.execute_with_meta_in(
            task,
            session_id,
            model,
            events,
            cancel,
            meta,
            None,
            Vec::new(),
            None,
            None,
            None,
        )
        .await
    }

    /// Like [`Self::execute_with_meta`] with an explicit session workspace
    /// root (stamped into the fresh header and used as the sandbox boundary).
    /// The plain variants delegate with `None`.
    ///
    /// `permission_preset` is a CREATION-TIME binding (BUG-113): on a fresh
    /// session the (sandbox, approval) bundle is appended right after the
    /// SessionCreated seeds and BEFORE the first turn starts, so the turn's
    /// sandbox/approval resolution reads the user's composer pick instead of
    /// the kernel's default. Resume turns ignore it (mid-session switches go
    /// through `session.setPermissionPreset`).
    #[allow(clippy::too_many_arguments)] // matches the seam's flat call style
    pub async fn execute_with_meta_in(
        &self,
        task: String,
        session_id: Option<String>,
        model: Option<String>,
        events: tokio::sync::broadcast::Sender<serde_json::Value>,
        cancel: tokio_util::sync::CancellationToken,
        meta: Option<HostSessionMeta>,
        cwd: Option<String>,
        images: Vec<starriver_kernel::llm::ContentBlock>,
        agent_preset: Option<String>,
        permission_preset: Option<crate::preset::Preset>,
        // 逐字节实时一致（开放回合快照流）：Some 时中继对结构事件计算
        // 开放回合投影并广播 TraceSnapshot；None 保持既有行为。
        trace: Option<TraceSnapContext>,
    ) -> anyhow::Result<KernelExecution> {
        let resume = session_id.is_some();
        let id = session_id
            .map(SessionId)
            .unwrap_or_else(|| SessionId(uuid::Uuid::new_v4().to_string()));
        let mut agent_options = self.default_agent_options(model);
        // Declared-text-only model (dsh pattern): request assembly must
        // reject any image block with an explicit error — the declaration is
        // a claim about the endpoint, and this enforces it before the wire.
        // The budgets are inert here (the reject check short-circuits before
        // projection); only the flag matters.
        if meta.as_ref().is_some_and(|m| m.reject_images) {
            agent_options.image_projection = Some(starriver_kernel::agent::ImageProjectionConfig {
                max_images: 0,
                max_request_bytes: 0,
                allow_reload: false,
                reject_images: true,
            });
        }
        // The execution-mode preset has already been pushed by the host into
        // self.preset via apply_execution_mode, and is stamped by SessionCreated
        // (fires only at creation). resume does not go through that event — so
        // the user's per-session approval choice (session.setApprovalMode) and
        // confirmPlan's unlock are not overridden. The host path always passes
        // Some(session_id); the `resume` flag cannot distinguish new vs existing
        // sessions. The execute path does not read the global settings file.
        let handle = if resume {
            self.registry
                .resume(ResumeAgentOptions {
                    resume_session_id: id.clone(),
                    // Unknown ids fall back to creation; stamp the workspace root.
                    cwd: cwd.clone(),
                    agent_options: agent_options.clone(),
                })
                .await?
        } else {
            self.registry
                .create(CreateAgentOptions {
                    session_id: id.clone(),
                    seed: Vec::new(),
                    cwd: cwd.clone(),
                    agent_preset: agent_preset.clone(),
                    agent_options: agent_options.clone(),
                })
                .await?
        };
        // BUG-113 creation-time permission binding: the composer's pre-send
        // pick rides execute. Stamp both knobs right after SessionCreated's
        // seeds and BEFORE the turn starts — the turn's sandbox/approval
        // resolution folds last-wins and reads the user's choice, not the
        // kernel default. Freshness is read off the LOG (no turn/start yet),
        // not off `session_id.is_some()`: the host generates the id itself
        // for new sessions, so `resume` is always true here and cannot
        // distinguish. A continuing session's log already has turn events →
        // its preset param is ignored (mid-session switches go through
        // session.setPermissionPreset).
        let fresh_log = !handle
            .agent
            .session()
            .events()
            .iter()
            .any(|e| e.event_type == "turn/start");
        if let Some(preset) = permission_preset.filter(|_| fresh_log) {
            let session = handle.agent.session();
            let _ = session.append(
                "sandbox/mode",
                starriver_kernel::json::JsonValue::Object(vec![(
                    "mode".into(),
                    starriver_kernel::json::JsonValue::String(preset.sandbox.as_str().into()),
                )]),
            );
            let _ = session.append(
                "approval/policy",
                starriver_kernel::json::JsonValue::Object(vec![(
                    "policy".into(),
                    starriver_kernel::json::JsonValue::String(preset.approval.as_str().into()),
                )]),
            );
        }
        // Plan strict lock to Manual: force-stamped every turn (new sessions
        // are already stamped at SessionCreated; this re-stamps idempotently;
        // existing resumed sessions are also locked here). The phase is the
        // folded `plan/mode` state (plan fold): no fold entry and
        // no legacy `plan/phase=execute` → entry flip (`plan/mode {active:
        // true}`) + read-only pairing; active → re-stamp read-only; inactive
        // (approved exit, this or legacy confirm) → workspace-write.
        // After switching to Plan, existing sessions are also locked on their
        // next turn.
        // Skipped when preset_override (explicit set_preset) is set —
        // tests/UI control the preset themselves.
        let plan_locked = !self
            .preset_override
            .load(std::sync::atomic::Ordering::SeqCst)
            && (*self.execution_mode.lock().unwrap())
                .is_some_and(crate::execution_mode::approval_locked);
        // plan fold: every publication re-registers the per-agent pre-step
        // listener that lands pending `plan/mode` flips (the host disposes
        // agents between turns, so each execute is a fresh context; the
        // context tracks the effect, dropping the handle is inert).
        let _plan_step_handle = self.plan_controller.register_step_listener(&handle.agent);
        if plan_locked {
            if let Some(agent) = self.registry.get(&SessionId(id.0.clone())) {
                let session = agent.session();
                if crate::plan_mode::plan_mode_fold(&session).is_none()
                    && !crate::plan_mode::legacy_plan_phase_confirmed(&session)
                {
                    // Entry journaling: the user picked the Plan profile; the log
                    // gains the binary state before the first stamped turn.
                    let _ = session.append(
                        crate::plan_mode::PLAN_MODE_EVENT,
                        starriver_kernel::json::JsonValue::Object(vec![(
                            "active".into(),
                            starriver_kernel::json::JsonValue::Bool(true),
                        )]),
                    );
                }
                let confirmed = !crate::plan_mode::effective_plan_mode(&session);
                // Sandbox: planning is forced read-only; a confirmed (approved
                // exit) session re-stamps its FOLDED sandbox when that is
                // within the Plan profile's allowed set — a review-card preset
                // (e.g. danger-full-access) must survive turn restarts — and
                // falls back to workspace-write to heal read-only residue.
                let sandbox_stamp = if confirmed {
                    match starriver_kernel::sandbox::effective_sandbox_mode(&session) {
                        Some(
                            mode @ (starriver_kernel::sandbox::SandboxMode::WorkspaceWrite
                            | starriver_kernel::sandbox::SandboxMode::DangerFullAccess),
                        ) => mode.as_str().to_string(),
                        _ => "workspace-write".to_string(),
                    }
                } else {
                    "read-only".to_string()
                };
                let _ = session.append(
                    "sandbox/mode",
                    starriver_kernel::json::JsonValue::Object(vec![(
                        "mode".into(),
                        starriver_kernel::json::JsonValue::String(sandbox_stamp),
                    )]),
                );
                // Approval: the planning phase stays hard-locked to Manual.
                // After an approved exit the user's explicit choice is the
                // authority (the review card writes approval/policy via
                // setPermissionPreset) — re-stamp the folded policy instead of
                // clobbering it back to Manual every turn (WI-144); a log with
                // no explicit choice yet folds to Manual (conservative default).
                let approval_stamp = if confirmed {
                    starriver_agent_loop::approval::effective_approval_mode(&session)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_else(|| "manual".to_string())
                } else {
                    "manual".to_string()
                };
                let _ = session.append(
                    "approval/policy",
                    starriver_kernel::json::JsonValue::Object(vec![(
                        "policy".into(),
                        starriver_kernel::json::JsonValue::String(approval_stamp),
                    )]),
                );
            }
        } else if !self
            .preset_override
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            // Reverse unlock (observed in game002, 2026-09-02): sessions
            // created/locked during Plan carry a read-only stamp; once the
            // global mode switches away (or the app restarts into a non-Plan
            // mode), the Plan confirmation card only renders when the UI's
            // mode==='plan', so it never appears — the session is permanently
            // locked read-only. Under non-Plan modes, re-stamp read-only
            // residue with the current preset, and exit any active plan
            // state — the user's mode switch is the authority, and a stale
            // `plan:policy` section would keep steering the model.
            if let Some(agent) = self.registry.get(&SessionId(id.0.clone())) {
                let session = agent.session();
                if crate::plan_mode::effective_plan_mode(&session) {
                    let _ = session.append(
                        crate::plan_mode::PLAN_MODE_EVENT,
                        starriver_kernel::json::JsonValue::Object(vec![(
                            "active".into(),
                            starriver_kernel::json::JsonValue::Bool(false),
                        )]),
                    );
                }
                if starriver_kernel::sandbox::effective_sandbox_mode(&session)
                    == Some(starriver_kernel::sandbox::SandboxMode::ReadOnly)
                {
                    let preset = *self.preset.lock().unwrap();
                    let _ = session.append(
                        "sandbox/mode",
                        starriver_kernel::json::JsonValue::Object(vec![(
                            "mode".into(),
                            starriver_kernel::json::JsonValue::String(
                                preset.sandbox.as_str().into(),
                            ),
                        )]),
                    );
                }
            }
        }
        // aborts the running turn (UiEvent::Cancelled) instead of the retired
        // RunningTasks bookkeeping. The select! on when_idle() lets the
        // watcher exit when the turn finishes first, so it never leaks.
        {
            let agent = Arc::clone(&handle.agent);
            let token = cancel.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = token.cancelled() => {
                        agent.cancel(
                            starriver_kernel::agent::AgentCancelCause::with_reason(
                                starriver_kernel::agent::AgentCancelKind::User,
                                "execute-cancel-token",
                            ),
                            false,
                        );
                    }
                    _ = agent.when_idle() => {}
                }
            });
        }
        // Host metadata lands in the log as non-surface events so the
        // model-visible conversation (derive_messages/surface) never sees it.
        if let Some(meta) = meta {
            if let Some(title) = meta.title {
                let _ = handle.agent.session().append_with(
                    "session/title",
                    starriver_kernel::json::JsonValue::Object(vec![
                        (
                            "title".into(),
                            starriver_kernel::json::JsonValue::String(title),
                        ),
                        (
                            "source".into(),
                            starriver_kernel::json::JsonValue::String("fallback".into()),
                        ),
                    ]),
                    starriver_kernel::session::AppendOptions::default(),
                );
            }
        }
        // SP4 (pulled forward): the per-execution `unix:` time contribution is
        // gone — its second-granularity value change defeated the change-only
        // runtime-context projection on every resume. A throttled time-context
        // snapshot arrives with the SP4 slice.
        let agent_ctx = handle.agent.ctx().clone();
        let _ = agent_ctx;
        // SessionStarted already fired inside create/resume publication; emit
        // it here so the caller's subscription (attached after) sees it.
        let _ = events.send(
            serde_json::to_value(UiEvent::SessionStarted {
                version: UiEvent::VERSION,
                session_id: id.0.clone(),
            })
            .unwrap(),
        );
        // Bridge subsequent session events onto the caller's channel.
        let bus = handle.agent.ctx().event_bus().cloned().unwrap();
        let agent = Arc::clone(&handle.agent);
        let events2 = events.clone();
        // Register listeners synchronously so no event is missed between the
        // publication above and the followup below.
        let mut stream = bridge::subscribe_ui_events(&bus, &agent);
        // 逐字节实时一致：结构事件后把开放回合投影作为快照广播（Some 时）。
        let mut snap_source = trace.as_ref().map(|ctx| TraceSnapSource {
            session_id: id.0.clone(),
            ctx: ctx.clone(),
            last_hash: None,
        });
        let agent_for_relay = Arc::clone(&agent);
        tokio::spawn(async move {
            // Send Now / queue FIFO: a terminal does not always end this
            // stream. The agent's own driver claims the next queued message
            // as a follow-up turn INSIDE the same execute round (the round
            // loop has kept the agent alive across those consecutive driver
            // turns since the Send Now fix) — breaking at the first terminal
            // starved the UI: the session looked terminated while the
            // follow-up ran headless. A terminal now ends the stream only
            // when the driver has no claimable work left; a parked queue
            // (manual stop) still ends it on purpose.
            let mut settle_poll = tokio::time::interval(std::time::Duration::from_millis(100));
            settle_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Consume the interval's immediate first tick; polls then run on
            // the 100ms cadence.
            settle_poll.tick().await;
            let mut awaiting_followup = false;
            loop {
                tokio::select! {
                    maybe = stream.recv() => match maybe {
                        Some(event) => {
                            let terminal = matches!(
                                event,
                                UiEvent::TurnEnd { .. }
                                    | UiEvent::TurnError { .. }
                                    | UiEvent::Cancelled { .. }
                            );
                            if let Ok(json) = serde_json::to_value(&event) {
                                if events2.send(json).is_err() {
                                    break;
                                }
                            }
                            // Compaction 重写历史：递增投影世代（客户端据此全量重拉），
                            // 本事件自身不产快照。
                            if matches!(event, UiEvent::Compaction { .. }) {
                                if let Some(s) = snap_source.as_mut() {
                                    s.bump_epoch();
                                }
                            } else if matches!(
                                &event,
                                UiEvent::ToolCall { .. }
                                    | UiEvent::ToolResult { .. }
                                    | UiEvent::AssistantText { .. }
                                    | UiEvent::ThinkingText { .. }
                                    | UiEvent::InstructionsBudget { .. }
                            ) {
                                if let Some(s) = snap_source.as_mut() {
                                    if let Some(snapshot) = s.compute().await {
                                        if let Ok(json) = serde_json::to_value(&snapshot) {
                                            if events2.send(json).is_err() {
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                            if terminal {
                                if driver_followup_pending(&agent_for_relay) {
                                    // The run continues (driver claims the
                                    // queued turn). The Idle flip emits no
                                    // UiEvent, so the poll arm below closes
                                    // the stream once the driver settles;
                                    // the follow-up's own terminal breaks
                                    // immediately in the normal case.
                                    awaiting_followup = true;
                                } else {
                                    // Dropping the stream disposes its bus
                                    // subscriptions. Without this break the
                                    // relay task (and its tx-holding handlers)
                                    // outlives the turn, so every later execute
                                    // on the shared agent bus receives this
                                    // session's events too — duplicates that
                                    // grow with each send.
                                    break;
                                }
                            }
                        }
                        None => break,
                    },
                    _ = settle_poll.tick(), if awaiting_followup => {
                        // Wedge guard: the driver flipped to idle without
                        // another terminal (claim cancelled / queue drained
                        // externally) — end the stream instead of hanging.
                        if !driver_followup_pending(&agent_for_relay) {
                            break;
                        }
                    }
                }
            }
        });
        // 逐字节实时一致：独立会话账本快照任务（user/message 即时、250ms 去抖、
        // turn/end 终帧）——UiEvent 中继触发不到 user/message，会让用户消息与
        // 首个思考行延迟到首个工具/正文事件（实测 30s+）。
        if let Some(ctx) = trace {
            tokio::spawn(Self::run_trace_snapshot_task(
                bus.clone(),
                events.clone(),
                ctx.inner.clone(),
                ctx.epochs.clone(),
                id.0.clone(),
            ));
        }
        // Position-faithful content: the host's extract_image_blocks now
        // returns the composed sequence (text segments interleaved with
        // Image blocks in the order the user wrote them) — appending the
        // images after the text reordered every image+text message. An
        // empty `images` still means a plain text message.
        let content = if images.is_empty() {
            vec![starriver_kernel::llm::ContentBlock::Text(task)]
        } else {
            images
        };
        handle.agent.followup(starriver_kernel::llm::Message {
            id: starriver_kernel::llm::MessageId(uuid::Uuid::new_v4().to_string()),
            role: starriver_kernel::llm::MessageRole::User,
            content,
            source: starriver_kernel::llm::MessageSource::User,
        });
        Ok(KernelExecution { handle })
    }

    /// 逐字节实时一致：会话账本变更 → 开放回合投影快照（250ms 去抖广播）。
    /// 独立于 UiEvent 中继：user/message 等账本事件不产生 UiEvent（translate
    /// 丢弃），中继触发会让「用户消息+首个思考行」延迟到首个工具/正文事件
    /// （实测 30s+）。这里以会话总线为准，250ms 内的变更合并为一次快照；
    /// turn/end 发终帧后退出。
    #[allow(clippy::too_many_arguments)]
    async fn run_trace_snapshot_task(
        bus: cordis_rslite::EventBus,
        events: tokio::sync::broadcast::Sender<serde_json::Value>,
        inner: Arc<tokio::sync::Mutex<Option<crate::RuntimeInner>>>,
        epochs: Arc<StdMutex<HashMap<String, u64>>>,
        session_id: String,
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let tx = Arc::new(tx);
        let _handle = bus.on_sync::<starriver_kernel::session::SessionEventOccurred, _>({
            let tx = Arc::clone(&tx);
            let sid = session_id.clone();
            move |occurred| {
                if occurred.session.id.0 != sid {
                    return Ok(cordis_rslite::Value::new(()));
                }
                let _ = tx.send(occurred.event.event_type.clone());
                Ok(cordis_rslite::Value::new(()))
            }
        });
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await; // 消耗立即触发的一次
        let mut dirty = false;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if !dirty { continue; }
                    dirty = false;
                    let l = inner.lock().await;
                    let Some(runtime) = l.as_ref() else { continue };
                    let Ok((mut entries, tool_names)) =
                        crate::sessions::load_projection_parts(runtime, &session_id).await
                    else { continue };
                    let Some((turn_index, slice)) = crate::sessions::open_turn_slice(entries) else { continue };
                    let epoch = *epochs.lock().unwrap_or_else(|e| e.into_inner()).get(&session_id).unwrap_or(&0);
                    let Some(snap_event) = crate::sessions::build_trace_snapshot(
                        &session_id, epoch, turn_index, slice, &tool_names,
                    ) else { continue };
                    if let Ok(json) = serde_json::to_value(&snap_event) {
                        if events.send(json).is_err() { break; }
                    }
                }
                ev = rx.recv() => {
                    match ev {
                        Some(t) => {
                            if t == "turn/end" { break; }
                            dirty = true;
                        }
                        None => break,
                    }
                }
            }
        }
    }

    fn default_agent_options(&self, model: Option<String>) -> AgentOptions {
        let caller = AgentOptions {
            compaction_tuning: None,
            provider: Some("mock".into()),
            model: model.clone().or_else(|| Some("mock".into())),
            ..Default::default()
        };
        // Backlog #7 settings hot-update: re-derive the preset from CURRENT
        // settings on every run instead of the startup-frozen registry, so a
        // settings write applies to the next execute without a restart. (The
        // per-run file re-read mirrors the max_tokens precedent below.)
        let settings = crate::settings::load_settings();
        let mut presets = HashMap::new();
        presets.insert("coding".to_string(), coding_preset(&settings));
        let mut options = starriver_agent_loop::agent_loop::resolve_preset_options(
            &presets,
            Some("coding"),
            caller,
        );
        // WI-159: cold/hot image projection — request budgets
        // (count cap + base64 wire bytes); the host bridges the attachment
        // store for hot resolution and byte accounting.
        options.image_projection = Some(starriver_kernel::agent::ImageProjectionConfig {
            max_images: settings
                .agent
                .image_max_request_images
                .unwrap_or_else(starriver_kernel::agent::ImageProjectionConfig::default_max_images),
            max_request_bytes: settings.agent.image_max_request_bytes.unwrap_or_else(
                starriver_kernel::agent::ImageProjectionConfig::default_max_request_bytes,
            ),
            allow_reload: true,
            reject_images: false,
        });
        options.image_resolver = self
            .ctx
            .require::<starriver_kernel::attachments::AttachmentStore>()
            .ok()
            .map(|store| {
                starriver_kernel::agent::SharedImageResolver(std::sync::Arc::new(
                    crate::attachments::StoreImageResolver::new((*store).clone()),
                ))
            });
        // WI-163: MCP availability feed — servers whose discovery failed (or
        // whose supervision gave up) ride a pre-step notice into the context
        // instead of silently vanishing from the model's tool vocabulary.
        options.mcp_status_feed = self
            .ctx
            .require::<crate::mcp_status_state::McpUnavailableServers>()
            .ok()
            .map(|state| {
                starriver_kernel::agent::SharedMcpStatusFeed(std::sync::Arc::new(move || {
                    state.snapshot()
                }))
            });
        // A1-3: a registered provider route wins over the preset's provider
        // (mock) so new agents ride the real engine the user activated. The
        // route must be the one OWNING the active model (switching to another
        // provider's model must not keep sending to the first-registered
        // route — WI-31 finding: cross-provider 400s with misleading copy).
        let owning_provider = model.as_deref().and_then(|m| {
            let settings = crate::settings::load_settings();
            provider_entry_for(&settings, m).map(|p| p.id.clone())
        });
        options.provider = Some(owning_provider.unwrap_or_else(|| self.default_provider()));
        // BUG-143: a null-model run used to carry the caller/preset's literal
        // "mock" to the wire (provider = first registered → the provider
        // answered "model not found", e.g. Zhipu 1211 模型不存在 on scheduled
        // Run Now). A run that reaches the wire without a concrete model now
        // resolves its provider's default model; the literal placeholder only
        // survives for the mock provider itself (tests).
        if options.model.as_deref().map(str::is_empty).unwrap_or(true)
            || options.model.as_deref() == Some("mock")
        {
            let settings = crate::settings::load_settings();
            let provider_id = options
                .provider
                .clone()
                .unwrap_or_else(|| self.default_provider());
            if let Some(entry) = settings.llm.providers.iter().find(|p| p.id == provider_id) {
                options.model = Some(entry.default_model.clone());
            }
        }
        // A user-configured per-provider max_tokens (model settings UI) is an
        // explicit choice and outranks the coding preset's default, so large
        // tool calls are not truncated at the preset floor (finish reason
        // max-tokens).
        if let Some(model_id) = model.as_deref() {
            let settings = crate::settings::load_settings();
            if let Some(value) = settings_max_tokens_for(&settings, model_id) {
                options.max_tokens = Some(value as u64);
            }
        }
        // WI-198: context-window fallback wiring. The preset only forwards
        // the EXPLICIT settings.agent.context_window; when neither the caller
        // nor settings declared a window, options.context_window stayed None
        // and the loop's compaction block (`if let Some(window) =
        // options.context_window`) never ran — context grew unbounded until
        // the provider choked (2026-10-14 incident: a zhipu session at
        // 102,743 tokens, past the 0.8×128k default threshold, with
        // compaction never armed). Resolve the window from the provider entry
        // owning the EFFECTIVE model (options.model, post BUG-143 default
        // fill; same ownership rules as max_tokens above): an entry-level
        // declaration wins, an entry without one gets the platform default
        // (128k — the same fallback generation_config() arms everywhere
        // else). Precedence: explicit settings.agent.context_window >
        // provider-declared window > 128k default. Providers unknown to
        // settings (the tests' mock) stay None — no window, no compaction,
        // exactly the preset-era behavior.
        if options.context_window.is_none() {
            let settings = crate::settings::load_settings();
            if let Some(model_id) = options.model.as_deref() {
                if let Some(window) = provider_entry_for(&settings, model_id)
                    .map(|p| p.generation_config().context_window)
                {
                    options.context_window = Some(window as u64);
                }
            }
        }
        let mcp_names = self.mcp_tool_names();
        let whitelist = self.review_whitelist.lock().unwrap().clone();
        apply_visible_tool_policies(options, &mcp_names, whitelist.as_deref())
    }
}

/// The provider entry owning `model_id` — a bare variant id or a
/// `providerId/variantId` composite key.
///
/// BUG-43: a composite key NAMES its provider; when two catalogs list the
/// same variant id the exact provider-id match must win, not whichever entry
/// comes first in the settings list (the variant-only search stays as the
/// bare-id fallback).
fn provider_entry_for<'a>(
    settings: &'a crate::settings::Settings,
    model_id: &str,
) -> Option<&'a crate::settings::LlmProviderSettings> {
    if let Some((provider_id, _)) = model_id.split_once('/') {
        if let Some(exact) = settings.llm.providers.iter().find(|p| p.id == provider_id) {
            return Some(exact);
        }
    }
    let variant_id = model_id.rsplit('/').next().unwrap_or(model_id);
    settings
        .llm
        .providers
        .iter()
        .find(|p| p.models.iter().any(|v| v.id == variant_id))
}

/// The per-provider max_tokens from settings (the model settings UI) for the
/// provider entry owning `model_id` — a bare variant id or a
/// `providerId/variantId` composite key.
fn settings_max_tokens_for(settings: &crate::settings::Settings, model_id: &str) -> Option<u32> {
    provider_entry_for(settings, model_id).and_then(|p| p.max_tokens)
}

/// Pure merge of the visible-tool policies: preset/caller set first, MCP
/// names extend it, and an active review whitelist replaces everything —
/// a review session sees exactly the review tools, MCP included.
fn apply_visible_tool_policies(
    mut options: AgentOptions,
    mcp_names: &[String],
    review_whitelist: Option<&[String]>,
) -> AgentOptions {
    match options.visible_tools.as_mut() {
        // An explicit whitelist is extended with MCP names.
        Some(visible) => visible.extend(mcp_names.iter().cloned()),
        // No whitelist = every registered tool is visible (registry
        // contract) — MCP tools included, they are registered too. Never
        // materialize a list here: None → Some([mcp…]) would HIDE every
        // builtin tool (read/bash/exit_plan_mode…) the moment any MCP server
        // is configured.
        None => {}
    }
    if let Some(whitelist) = review_whitelist {
        options.visible_tools = Some(whitelist.to_vec());
    }
    options
}

/// Run one bounded subagent turn (subagent in-process driver parity):
/// a fresh agent session executes `prompt` under the template's bound model,
/// the calling agent's inherited route, or the preset default, and returns
/// the assistant's text. Used by the `subagent_spawn` tool; the sub-session
/// is disposed when the turn settles, so nothing leaks into the caller's
/// session store.
impl PluginKernel {
    pub async fn spawn_subagent(
        &self,
        prompt: String,
        model_key: Option<String>,
        inherited: Option<(String, String)>,
    ) -> anyhow::Result<String> {
        let id = SessionId(uuid::Uuid::new_v4().to_string());
        let (provider, model_name) = match model_key {
            Some(key) => {
                let providers = self.llm.registered_providers();
                let refs: Vec<&str> = providers.iter().map(|s| s.as_str()).collect();
                match crate::split_model_key(&key, &refs) {
                    Some((pid, vid)) => (Some(pid), Some(vid.to_string())),
                    None => (None, Some(key)),
                }
            }
            None => (None, None),
        };
        let mut options = self.default_agent_options(model_name);
        if let Some(provider) = provider {
            options.provider = Some(provider);
        } else if let Some((inherited_provider, inherited_model)) = inherited {
            // parent-inheritance parity: an unbound template runs the
            // child on the calling agent's live route instead of the preset
            // placeholder.
            options.provider = Some(inherited_provider);
            options.model = Some(inherited_model);
        }
        let handle = self
            .registry
            .create(CreateAgentOptions {
                session_id: id.clone(),
                seed: Vec::new(),
                cwd: None,
                agent_preset: None,
                agent_options: options,
            })
            .await?;
        let bus = handle.agent.ctx().event_bus().cloned().expect("agent bus");
        let mut stream = bridge::subscribe_ui_events(&bus, &handle.agent);
        handle.agent.followup(starriver_kernel::llm::Message {
            id: starriver_kernel::llm::MessageId(uuid::Uuid::new_v4().to_string()),
            role: starriver_kernel::llm::MessageRole::User,
            content: vec![starriver_kernel::llm::ContentBlock::Text(prompt)],
            source: starriver_kernel::llm::MessageSource::User,
        });
        let mut text = String::new();
        while let Some(event) = stream.recv().await {
            match event {
                UiEvent::AssistantText { text: part, .. } => text.push_str(&part),
                UiEvent::TurnEnd { .. } => break,
                UiEvent::TurnError { error, .. } => {
                    handle.dispose();
                    return Err(anyhow::anyhow!("subagent failed: {error}"));
                }
                UiEvent::Cancelled { .. } => break,
                _ => {}
            }
        }
        handle.dispose();
        Ok(text.trim().to_string())
    }
}

/// True while the agent's driver still has claimable work or a live run:
/// the run continues at the next turn boundary (queue FIFO / a steer that
/// landed at turn end). Mirrors the driver's own continue condition —
/// `drive()` opens another turn whenever the inbox has pending input and no
/// manual stop parked the queue; `Running` covers the settle window where
/// the claimed message has already left the inbox but the phase has not
/// flipped to Idle yet.
fn driver_followup_pending(agent: &std::sync::Arc<dyn starriver_kernel::agent::Agent>) -> bool {
    use starriver_kernel::agent::AgentStatus;
    if agent.is_parked() {
        return false;
    }
    let pending = agent.pending_messages();
    if !pending.next_turn.is_empty() || !pending.next_step.is_empty() {
        return true;
    }
    agent.status() == AgentStatus::Running
}

/// One launched agent execution: consumer handle plus a UiEvent stream
/// already bridged onto the caller's broadcast channel.
pub struct KernelExecution {
    handle: starriver_kernel::agent::AgentHandle,
}

impl KernelExecution {
    pub fn agent(&self) -> &Arc<dyn Agent> {
        &self.handle.agent
    }

    pub fn dispose(self) {
        self.handle.dispose();
    }
}

/// Wait until the turn settles (turnEnd/turnError/Cancelled). Owns its
/// arguments (no `&self` capture) so the future stays `Send`.
pub async fn wait_terminal(agent: Arc<dyn Agent>, bus: cordis_rslite::EventBus) {
    let mut stream = bridge::subscribe_ui_events(&bus, &agent);
    while let Some(event) = stream.recv().await {
        match event {
            UiEvent::TurnEnd { .. } | UiEvent::TurnError { .. } | UiEvent::Cancelled { .. } => {
                return;
            }
            _ => {}
        }
    }
}

pub mod bridge {
    use std::sync::Arc;
    use tokio::sync::mpsc;

    use cordis_rslite::{EventBus, Value};
    use starriver_kernel::agent::{Agent, AgentSessionStart};
    use starriver_kernel::json::JsonValue;
    use starriver_kernel::llm::Message;
    use starriver_kernel::session::{SessionEvent, SessionEventOccurred};
    use starriver_kernel::ui::UiEvent;

    /// Subscribe the agent's bus and translate cordis events to UiEvent.
    pub fn subscribe_ui_events(bus: &EventBus, agent: &Arc<dyn Agent>) -> UiEventStream {
        let (tx, rx) = mpsc::unbounded_channel();
        let session_id = agent.id().0.clone();
        let session_handle = bus.on_sync::<SessionEventOccurred, _>({
            let tx = tx.clone();
            let session_id = session_id.clone();
            move |occurred| {
                // The host bus is shared by EVERY session (interactive,
                // scheduled tick, subagents). Unowned on_sync listeners see
                // them all — drop foreign sessions' events or they are
                // mislabeled with this session's id, and a foreign turn/end
                // breaks the relay / makes wait_terminal dispose a LIVE turn.
                if occurred.session.id.0 != session_id {
                    return Ok(Value::new(()));
                }
                for event in translate(&occurred.event, &session_id) {
                    let _ = tx.send(event);
                }
                Ok(Value::new(()))
            }
        });
        let start_handle = bus.on_sync::<AgentSessionStart, _>({
            let tx = tx.clone();
            let session_id = session_id.clone();
            move |started| {
                if started.agent.id().0 != session_id {
                    return Ok(Value::new(()));
                }
                let _ = tx.send(UiEvent::SessionStarted {
                    version: UiEvent::VERSION,
                    session_id: started.agent.id().0.clone(),
                });
                Ok(Value::new(()))
            }
        });
        UiEventStream {
            rx,
            _session_handle: session_handle,
            _start_handle: start_handle,
        }
    }

    /// A UiEvent stream that keeps its bus listeners alive for as long as the
    /// receiver is held. Dropping the stream unsubscribes the listeners.
    pub struct UiEventStream {
        rx: mpsc::UnboundedReceiver<UiEvent>,
        _session_handle: cordis_rslite::EffectHandle,
        _start_handle: cordis_rslite::EffectHandle,
    }

    impl UiEventStream {
        pub async fn recv(&mut self) -> Option<UiEvent> {
            self.rx.recv().await
        }
    }

    pub(crate) fn translate(event: &SessionEvent, session_id: &str) -> Vec<UiEvent> {
        match event.event_type.as_str() {
            // The between-turns marker (queue Send Now / FIFO auto-claim run
            // follow-up turns inside the SAME execute — no second
            // SessionStarted). Live UIs key per-turn re-arm on it.
            "turn/start" => vec![UiEvent::TaskStarted {
                version: UiEvent::VERSION,
                session_id: session_id.into(),
            }],
            // WI-35: the instructions owned-message carries the budget state
            // in its plugin form; a truncated composite surfaces as a UiEvent
            // so UIs can mark the context as clipped (design §4 A).
            "user/message" => {
                // The owned message IS the event payload (no nesting).
                let Ok(message) = Message::from_json(event.data.clone()) else {
                    return Vec::new();
                };
                let starriver_kernel::llm::MessageSource::Plugin { plugin, form } = &message.source
                else {
                    return Vec::new();
                };
                if plugin != "agent-instructions" {
                    return Vec::new();
                }
                let Some(starriver_kernel::llm::ContextForm::Snapshot {
                    truncated,
                    budget_bytes,
                    composed_bytes,
                    sections,
                }) = form
                else {
                    return Vec::new();
                };
                if !*truncated {
                    return Vec::new();
                }
                vec![UiEvent::InstructionsBudget {
                    version: UiEvent::VERSION,
                    session_id: session_id.into(),
                    truncated: true,
                    total_sections: sections.len() as u32,
                    budget_bytes: *budget_bytes,
                    composed_bytes: *composed_bytes,
                }]
            }
            "assistant/message" => {
                let Ok(message) = Message::from_json(
                    event
                        .data
                        .get("message")
                        .cloned()
                        .unwrap_or(JsonValue::Null),
                ) else {
                    return Vec::new();
                };
                let mut text = String::new();
                let mut thinking = String::new();
                for block in &message.content {
                    match block {
                        starriver_kernel::llm::ContentBlock::Text(t) => text.push_str(t),
                        starriver_kernel::llm::ContentBlock::Reasoning(t) => thinking.push_str(t),
                        _ => {}
                    }
                }
                let mut out = Vec::new();
                if !thinking.is_empty() {
                    out.push(UiEvent::ThinkingText {
                        version: UiEvent::VERSION,
                        session_id: session_id.into(),
                        text: thinking,
                    });
                }
                if !text.is_empty() {
                    out.push(UiEvent::AssistantText {
                        version: UiEvent::VERSION,
                        session_id: session_id.into(),
                        text,
                    });
                }
                // Per-step token usage rides the message payload; project it
                // as its own event so live UIs can show spend (cache split /
                // reasoning tokens included when the provider reports them).
                if let Some(usage) = event.data.get("usage") {
                    let num = |k: &str| usage.get(k).and_then(JsonValue::as_u64);
                    if let (Some(input_tokens), Some(output_tokens)) =
                        (num("inputTokens"), num("outputTokens"))
                    {
                        out.push(UiEvent::Usage {
                            version: UiEvent::VERSION,
                            session_id: session_id.into(),
                            input_tokens,
                            output_tokens,
                            cache_read_tokens: num("cacheReadTokens"),
                            cache_write_tokens: num("cacheWriteTokens"),
                            reasoning_tokens: num("reasoningTokens"),
                        });
                    }
                }
                out
            }
            "assistant/chunk" => {
                // Forward reasoning deltas so the UI can stream the thinking
                // bubble, and text deltas so it can typewriter the reply;
                // the finished ThinkingText/AssistantText still land with
                // the message (renderers that fed on deltas dedupe them).
                let chunk = event.data.get("chunk");
                let chunk_type = chunk
                    .and_then(|c| c.get("type"))
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default();
                match chunk
                    .and_then(|c| c.get("text"))
                    .and_then(JsonValue::as_str)
                {
                    Some(text) if !text.is_empty() => match chunk_type {
                        "reasoning-delta" => vec![UiEvent::ThinkingDelta {
                            version: UiEvent::VERSION,
                            session_id: session_id.into(),
                            text: text.into(),
                        }],
                        "text-delta" => vec![UiEvent::AssistantTextDelta {
                            version: UiEvent::VERSION,
                            session_id: session_id.into(),
                            text: text.into(),
                        }],
                        _ => vec![],
                    },
                    _ => vec![],
                }
            }
            "tool/call" => vec![UiEvent::ToolCall {
                version: UiEvent::VERSION,
                session_id: session_id.into(),
                id: event.data.as_str_at("callId").unwrap_or_default().into(),
                name: event.data.as_str_at("name").unwrap_or_default().into(),
                // WI-47 Phase B dual-format: `arguments` rides as a string
                // (old logs) or the lossless object (new logs) — UIs get JSON
                // text either way so they can render the actual command
                // (e.g. `bash: ls`, `read: path`) instead of the bare name.
                input: match event.data.get("arguments") {
                    Some(v) if v.as_str().is_some() => v.as_str().map(|s| s.to_string()),
                    Some(other) => other.to_serde_json().ok().map(|j| j.to_string()),
                    None => None,
                },
            }],
            "tool/result" => {
                // BUG-147: compaction's prune phase rewrites an OLD tool
                // result in place (`SurfaceOp::Replace` — ledger bookkeeping
                // shadowing the original, which stays in the log). That is
                // not new activity: forwarding it as a fresh
                // `UiEvent::ToolResult` made the desktop render an orphan
                // "Tool / 成功" card no live ToolCallStart could pair with.
                // Fresh results ride `Some(Append)` and keep flowing.
                if matches!(
                    event.surface_op,
                    Some(starriver_kernel::session::SurfaceOp::Replace { .. })
                ) {
                    return Vec::new();
                }
                let Ok(message) = Message::from_json(
                    event
                        .data
                        .get("message")
                        .cloned()
                        .unwrap_or(JsonValue::Null),
                ) else {
                    return Vec::new();
                };
                let mut output = String::new();
                for block in &message.content {
                    match block {
                        starriver_kernel::llm::ContentBlock::Text(t) => output.push_str(t),
                        starriver_kernel::llm::ContentBlock::ToolResult { content, .. } => {
                            // Tool-result messages wrap the rendered blocks in
                            // a ToolResult block; surface their text so the
                            // UI event carries what the model saw.
                            for inner in content {
                                if let starriver_kernel::llm::ContentBlock::Text(t) = inner {
                                    output.push_str(t);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                vec![UiEvent::ToolResult {
                    version: UiEvent::VERSION,
                    session_id: session_id.into(),
                    id: match &message.source {
                        starriver_kernel::llm::MessageSource::Tool { call_id } => call_id.0.clone(),
                        _ => String::new(),
                    },
                    ok: !event.data.get("error").is_some(),
                    output,
                    // Opaque tool-owned card payload (backlog #6): pass
                    // through verbatim for client-side narrowing.
                    meta: event
                        .data
                        .get("meta")
                        .and_then(|meta| meta.to_serde_json().ok()),
                }]
            }
            "compaction/summary" => {
                // Context compaction checkpoint landed: surface the fold row
                // with the checkpoint Markdown (compaction/summary
                // parity). start/end bookkeeping events stay silent.
                let summary = event
                    .data
                    .as_str_at("summary")
                    .unwrap_or_default()
                    .to_string();
                if summary.trim().is_empty() {
                    return Vec::new();
                }
                let num = |k: &str| event.data.get(k).and_then(JsonValue::as_u64);
                vec![UiEvent::Compaction {
                    version: UiEvent::VERSION,
                    session_id: session_id.into(),
                    summary,
                    shadowed_start: num("shadowedStart").unwrap_or(0),
                    shadowed_end: num("shadowedEnd").unwrap_or(0),
                }]
            }
            "compaction/start" | "compaction/end" => Vec::new(),
            "turn/end" => {
                let reason = event
                    .data
                    .get("reason")
                    .and_then(|r| r.as_str_at("kind"))
                    .unwrap_or("completed");
                let summary = event
                    .data
                    .get("reason")
                    .and_then(|r| r.get("failure"))
                    .and_then(|f| f.as_str_at("message"))
                    .unwrap_or("")
                    .to_string();
                match reason {
                    "error" => vec![UiEvent::TurnError {
                        version: UiEvent::VERSION,
                        session_id: session_id.into(),
                        error: summary,
                    }],
                    "aborted" | "disposed" => vec![UiEvent::Cancelled {
                        version: UiEvent::VERSION,
                        session_id: session_id.into(),
                    }],
                    _ => vec![UiEvent::TurnEnd {
                        version: UiEvent::VERSION,
                        session_id: session_id.into(),
                        reason: reason.into(),
                        summary,
                    }],
                }
            }
            "interaction/question" => vec![UiEvent::QuestionAsked {
                version: UiEvent::VERSION,
                session_id: session_id.into(),
                question_id: event.data.as_str_at("id").unwrap_or_default().into(),
                text: event.data.as_str_at("text").unwrap_or_default().into(),
                options: event
                    .data
                    .get("options")
                    .and_then(JsonValue::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                timeout_secs: event
                    .data
                    .get("timeoutSecs")
                    .and_then(JsonValue::as_u64)
                    .unwrap_or(0),
            }],
            "plan/review-requested" => vec![UiEvent::PlanReviewRequested {
                version: UiEvent::VERSION,
                session_id: session_id.into(),
                request_id: event.data.as_str_at("id").unwrap_or_default().into(),
                plan: event.data.as_str_at("plan").unwrap_or_default().into(),
            }],
            "plan/review-decided" => vec![UiEvent::PlanReviewDecided {
                version: UiEvent::VERSION,
                session_id: session_id.into(),
                request_id: event.data.as_str_at("id").unwrap_or_default().into(),
                approved: event
                    .data
                    .get("approved")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false),
                abandoned: event
                    .data
                    .get("abandoned")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false),
                feedback: event.data.as_str_at("feedback").map(String::from),
            }],
            "approval/asked" => vec![UiEvent::ApprovalRequested {
                version: UiEvent::VERSION,
                session_id: session_id.into(),
                request_id: event.data.as_str_at("id").unwrap_or_default().into(),
                tool_name: event.data.as_str_at("toolName").unwrap_or_default().into(),
                call_id: event
                    .data
                    .get("callId")
                    .and_then(JsonValue::as_str)
                    .map(String::from),
                reason: event
                    .data
                    .get("reason")
                    .and_then(JsonValue::as_str)
                    .map(String::from),
                // Arguments text lets approval UIs show the actual command
                // before the user decides.
                params: event.data.as_str_at("arguments").map(String::from),
            }],
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    /// BUG-147: compaction prune rewrites an old tool result in place
    /// (SurfaceOp::Replace). The relay must not surface that rewrite as a
    /// fresh UiEvent::ToolResult — the desktop rendered it as an orphan
    /// "Tool / 成功" card no live ToolCallStart could pair with. Fresh
    /// (Append) results keep flowing with their call identity.
    #[test]
    fn translate_drops_prune_rewritten_tool_results() {
        use starriver_kernel::llm::{
            CallId, ContentBlock, Message, MessageId, MessageRole, MessageSource,
        };
        use starriver_kernel::session::{SessionEvent, SurfaceOp};

        let message = Message {
            id: MessageId("m1".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                call_id: CallId("c-old".into()),
                content: vec![ContentBlock::Text(
                    "[head retained]\n[earlier output pruned]\n[tail retained]".into(),
                )],
                is_error: false,
            }],
            source: MessageSource::Tool {
                call_id: CallId("c-old".into()),
            },
        };
        let event = |surface_op| SessionEvent {
            seq: 1,
            time: 0,
            event_type: "tool/result".into(),
            data: starriver_kernel::json::JsonValue::Object(vec![(
                "message".into(),
                message.to_json().unwrap(),
            )]),
            surface_op,
            source_event_seqs: Vec::new(),
        };

        // The prune rewrite (Replace) is ledger bookkeeping — never a fresh
        // tool result for the UI.
        let rewritten = event(Some(SurfaceOp::Replace { start: 7, end: 7 }));
        assert!(
            bridge::translate(&rewritten, "s1").is_empty(),
            "a SurfaceOp::Replace tool/result must not forward to the UI"
        );

        // Fresh results (Append) keep flowing with their call identity.
        let fresh = event(Some(SurfaceOp::Append));
        let out = bridge::translate(&fresh, "s1");
        assert_eq!(out.len(), 1);
        match &out[0] {
            UiEvent::ToolResult { id, ok, .. } => {
                assert_eq!(id, "c-old");
                assert!(*ok);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    /// WI-35: a truncated instructions owned-message surfaces as an
    /// InstructionsBudget UiEvent; untruncated/foreign plugin messages don't.
    #[test]
    fn translate_instructions_truncation_to_budget_event() {
        use starriver_kernel::llm::{
            ContentBlock, ContextForm, ContextSnapshotSection, Message, MessageId, MessageRole,
            MessageSource,
        };
        use starriver_kernel::session::SessionEvent;

        let mk = |truncated: bool| {
            let message = Message {
                id: MessageId("m-i".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::Text("instructions".into())],
                source: MessageSource::Plugin {
                    plugin: "agent-instructions".into(),
                    form: Some(starriver_kernel::llm::ContextForm::Snapshot {
                        sections: vec![ContextSnapshotSection {
                            name: "AGENTS.md".into(),
                            text: "rules".into(),
                        }],
                        truncated,
                        budget_bytes: 65_536,
                        composed_bytes: 90_000,
                    }),
                },
            };
            SessionEvent {
                seq: 1,
                time: 0,
                event_type: "user/message".into(),
                data: message.to_json().expect("instructions message serializes"),
                surface_op: None,
                source_event_seqs: vec![],
            }
        };

        let out = bridge::translate(&mk(true), "s1");
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(matches!(
            out[0],
            UiEvent::InstructionsBudget {
                truncated: true,
                total_sections: 1,
                budget_bytes: 65_536,
                ..
            }
        ));

        let out = bridge::translate(&mk(false), "s1");
        assert!(out.is_empty(), "untruncated stays silent");
    }

    use super::*;
    use futures::stream::BoxStream;
    use starriver_kernel::llm::{GenerateOptions, LlmAdapter, LlmError, StreamChunk};

    fn bus_and_ctx() -> (cordis_rslite::EventBus, cordis_rslite::Context) {
        let bus = cordis_rslite::EventBus::new();
        let ctx = cordis_rslite::Context::new().with_event_bus(&bus);
        (bus, ctx)
    }

    fn temp_persistence(
        bus: &cordis_rslite::EventBus,
    ) -> Arc<starriver_session_persistence::SqliteSessionPersistence> {
        Arc::new(
            starriver_session_persistence::SqliteSessionPersistence::open_with_bus(
                std::env::temp_dir().join(format!("pk-{}.db", uuid::Uuid::new_v4())),
                bus.clone(),
            )
            .unwrap(),
        )
    }

    /// Adapter that self-declares its `deepseek` route, like the protocol
    /// adapters after pluginization.
    struct DeclaredAdapter;
    impl LlmAdapter for DeclaredAdapter {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["deepseek".into()])
        }

        fn stream(
            &self,
            _options: GenerateOptions,
        ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
            Box::pin(futures::stream::empty())
        }
    }

    #[tokio::test]
    async fn plugin_kernel_composes_registry_loop_and_persistence() {
        let (bus, ctx) = bus_and_ctx();
        let persistence = temp_persistence(&bus);
        let kernel = PluginKernel::new(ctx, bus, persistence);
        assert!(kernel.registry().list().is_empty());
    }

    #[tokio::test]
    async fn self_declaring_adapter_registers_route_and_becomes_default() {
        let (bus, ctx) = bus_and_ctx();
        let persistence = temp_persistence(&bus);
        let kernel = PluginKernel::new(ctx, bus, persistence);

        // A self-declaring adapter registers under its own provider id and
        // becomes the new-agent default (regression: route was silently
        // dropped, so new agents fell back to provider `mock`).
        kernel.register_provider_adapter(Arc::new(DeclaredAdapter));
        assert_eq!(
            kernel.debug_provider_routes(),
            vec!["deepseek".to_string()],
            "self-declared route must be served under the adapter's provider id"
        );
        assert_eq!(
            kernel.default_provider(),
            "deepseek",
            "registered route must win the new-agent default provider"
        );
    }

    #[test]
    fn assistant_message_with_reasoning_emits_both_thinking_and_text() {
        use starriver_kernel::json::JsonValue;
        use starriver_kernel::session::SessionEvent;
        use starriver_kernel::ui::UiEvent;

        let block = |kind: &str, text: &str| {
            JsonValue::Object(vec![
                ("type".into(), JsonValue::String(kind.into())),
                ("text".into(), JsonValue::String(text.into())),
            ])
        };
        let event = SessionEvent {
            seq: 1,
            time: 1,
            event_type: "assistant/message".into(),
            data: JsonValue::Object(vec![(
                "message".into(),
                JsonValue::Object(vec![
                    ("role".into(), JsonValue::String("assistant".into())),
                    (
                        "content".into(),
                        JsonValue::Array(vec![
                            block("reasoning", "think step"),
                            block("text", "final answer"),
                        ]),
                    ),
                    (
                        "source".into(),
                        JsonValue::Object(vec![("kind".into(), JsonValue::String("model".into()))]),
                    ),
                ]),
            )]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        };

        let events = bridge::translate(&event, "s1");
        assert_eq!(
            events.len(),
            2,
            "a reasoning+text assistant message must emit both events: {events:?}"
        );
        assert!(matches!(
            &events[0],
            UiEvent::ThinkingText { text, .. } if text == "think step"
        ));
        assert!(matches!(
            &events[1],
            UiEvent::AssistantText { text, .. } if text == "final answer"
        ));
    }

    #[test]
    fn settings_max_tokens_resolves_provider_entry_for_variant() {
        let settings: crate::settings::Settings = serde_json::from_value(serde_json::json!({
            "llm": {
                "providers": [
                    {
                        "id": "deepseek",
                        "name": "DeepSeek",
                        "model_type": "cloud",
                        "provider_type": "openai-compat",
                        "default_model": "deepseek-v4-flash",
                        "models": [{ "id": "deepseek-v4-flash", "name": "V4 Flash" }],
                        "max_tokens": 16384
                    },
                    {
                        "id": "other",
                        "name": "Other",
                        "model_type": "cloud",
                        "provider_type": "openai-compat",
                        "default_model": "m1",
                        "models": [{ "id": "m1", "name": "M1" }]
                    }
                ]
            }
        }))
        .unwrap();

        // Bare variant id resolves the owning provider's max_tokens...
        assert_eq!(
            settings_max_tokens_for(&settings, "deepseek-v4-flash"),
            Some(16384)
        );
        // ...as does a provider/variant composite key.
        assert_eq!(
            settings_max_tokens_for(&settings, "deepseek/deepseek-v4-flash"),
            Some(16384)
        );
        // A variant whose provider has no max_tokens configured, and an
        // unknown variant, both fall through to the preset default.
        assert_eq!(settings_max_tokens_for(&settings, "m1"), None);
        assert_eq!(settings_max_tokens_for(&settings, "nope"), None);
    }

    /// BUG-43 regression: a `providerId/variantId` composite key NAMES its
    /// provider — when two provider catalogs list the same variant id, the
    /// named provider must win, not whichever entry happens to come first in
    /// the settings list (the `owning_provider` route resolver shares the
    /// same lookup and would silently send requests to the wrong provider).
    #[test]
    fn settings_max_tokens_composite_key_honors_the_named_provider() {
        let settings: crate::settings::Settings = serde_json::from_value(serde_json::json!({
            "llm": {
                "providers": [
                    {
                        "id": "proxy",
                        "name": "Proxy",
                        "model_type": "cloud",
                        "provider_type": "openai-compat",
                        "default_model": "gpt-4o",
                        "models": [{ "id": "gpt-4o", "name": "GPT" }],
                        "max_tokens": 4096
                    },
                    {
                        "id": "openai",
                        "name": "OpenAI",
                        "model_type": "cloud",
                        "provider_type": "openai-compat",
                        "default_model": "gpt-4o",
                        "models": [{ "id": "gpt-4o", "name": "GPT" }],
                        "max_tokens": 8192
                    }
                ]
            }
        }))
        .unwrap();

        // The composite key names openai; proxy merely comes first in the list.
        assert_eq!(
            settings_max_tokens_for(&settings, "openai/gpt-4o"),
            Some(8192),
            "the named provider must win over list order"
        );
        // Bare variant keeps the documented first-match behavior.
        assert_eq!(settings_max_tokens_for(&settings, "gpt-4o"), Some(4096));
    }

    #[test]
    fn visible_tool_policies_mcp_extends_then_review_replaces() {
        let mcp = vec!["mcp_jira_search".to_string()];
        let options = AgentOptions {
            visible_tools: Some(vec!["bash".into(), "read".into()]),
            ..Default::default()
        };
        let merged = apply_visible_tool_policies(options.clone(), &mcp, None);
        let visible = merged.visible_tools.unwrap();
        assert!(visible.contains(&"bash".into()) && visible.contains(&"mcp_jira_search".into()));

        let whitelist = vec!["read".to_string(), "grep".to_string()];
        let restricted = apply_visible_tool_policies(options, &mcp, Some(whitelist.as_slice()));
        assert_eq!(
            restricted.visible_tools.as_deref(),
            Some(whitelist.as_slice()),
            "review whitelist must also suppress MCP additions"
        );
    }

    #[test]
    fn visible_tool_policies_none_stays_none_when_mcp_present() {
        // None = everything visible (registry contract). Configuring an
        // MCP server must NOT narrow the surface to MCP-only — that collapse
        // once hid read/bash/exit_plan_mode from every session.
        let options = AgentOptions {
            visible_tools: None,
            ..Default::default()
        };
        let mcp = vec!["mcp_devops_list_repos".to_string()];
        let merged = apply_visible_tool_policies(options, &mcp, None);
        assert!(
            merged.visible_tools.is_none(),
            "None must stay None — MCP tools are registered and visible already"
        );
    }

    #[tokio::test]
    async fn review_whitelist_replaces_visible_tools_until_exit() {
        let (bus, ctx) = bus_and_ctx();
        let persistence = temp_persistence(&bus);
        let kernel = PluginKernel::new(ctx, bus, persistence);

        let baseline = kernel.default_agent_options(None);
        assert!(
            baseline.visible_tools.is_none(),
            "preset ships no whitelist — every registered tool is visible"
        );

        kernel
            .enter_review_mode(
                "**REVIEW**".into(),
                Some(vec!["read".into(), "grep".into()]),
            )
            .await
            .unwrap();
        let restricted = kernel.default_agent_options(None);
        assert_eq!(
            restricted.visible_tools.as_deref(),
            Some(&["read".to_string(), "grep".to_string()][..]),
            "review whitelist replaces the whole visible set"
        );

        kernel.exit_review_mode().await;
        let restored = kernel.default_agent_options(None);
        assert!(
            restored.visible_tools.is_none(),
            "exiting review releases the whitelist — back to register-equals-visible"
        );
    }

    #[tokio::test]
    async fn default_agent_options_resolves_context_window_fallback() {
        use crate::testutil::with_temp_home;

        // WI-198 regression: the loop's compaction block is
        // `if let Some(window) = options.context_window` — an unset window
        // silently disabled compaction for every provider that never
        // declared one (2026-10-14 incident: a zhipu session grew to
        // 102,743 tokens, past the 0.8×128k default threshold, with
        // compaction never armed). default_agent_options must arm the
        // window: settings.agent.context_window > provider declaration >
        // 128k platform default.
        let (bus, ctx) = bus_and_ctx();
        let persistence = temp_persistence(&bus);
        let kernel = PluginKernel::new(ctx, bus, persistence);

        let write_settings = |home: &std::path::Path, yaml: &str| {
            let dir = home.join(".starriver");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("settings.yaml"), yaml).unwrap();
        };
        let provider_yaml = |window: Option<u32>| {
            let window_line = window
                .map(|w| format!("      context_window: {w}\n"))
                .unwrap_or_default();
            format!(
                "llm:\n  providers:\n    - id: zhipu\n      name: Zhipu\n      model_type: cloud\n      provider_type: openai-compat\n      base_url: https://open.bigmodel.example\n      api_key: \"!file:model/zhipu\"\n      default_model: glm-4.7\n{window_line}      models:\n        - id: glm-4.7\n          name: GLM-4.7\n"
            )
        };

        with_temp_home(|home| {
            // 1) A provider that never declared a window gets the platform
            //    default — the compaction threshold (0.8×128k) is armed.
            write_settings(home, &provider_yaml(None));
            let options = kernel.default_agent_options(Some("glm-4.7".into()));
            assert_eq!(
                options.context_window,
                Some(128_000),
                "undeclared provider window must fall back to the 128k default \
                 so the loop's compaction block actually runs"
            );

            // 2) A provider-declared window outranks the implicit default.
            write_settings(home, &provider_yaml(Some(256_000)));
            let options = kernel.default_agent_options(Some("glm-4.7".into()));
            assert_eq!(options.context_window, Some(256_000));

            // 3) The explicit settings.agent.context_window still wins over
            //    the provider declaration (preset passthrough precedence).
            write_settings(
                home,
                &format!(
                    "{}agent:\n  context_window: 65536\n",
                    provider_yaml(Some(256_000))
                ),
            );
            let options = kernel.default_agent_options(Some("glm-4.7".into()));
            assert_eq!(options.context_window, Some(65_536));
        });
    }

    #[tokio::test]
    async fn apply_execution_mode_refreshes_preset_per_mode() {
        use crate::execution_mode::ExecutionMode;
        use crate::preset::Preset;
        use starriver_kernel::sandbox::SandboxMode;

        let (bus, ctx) = bus_and_ctx();
        let persistence = temp_persistence(&bus);
        let kernel = PluginKernel::new(ctx, bus, persistence);

        // Plan planning-phase preset: read-only + Manual (the phase lock is stamped on the execute path).
        kernel.apply_execution_mode(Some(ExecutionMode::Plan));
        assert_eq!(kernel.debug_preset().sandbox, SandboxMode::ReadOnly);
        assert_eq!(
            kernel.debug_preset().approval,
            starriver_agent_loop::approval::ApprovalMode::Manual
        );

        // PTC / Goal: workspace-write + Auto.
        kernel.apply_execution_mode(Some(ExecutionMode::Ptc));
        assert_eq!(kernel.debug_preset().sandbox, SandboxMode::WorkspaceWrite);
        assert_eq!(
            kernel.debug_preset().approval,
            starriver_agent_loop::approval::ApprovalMode::Auto
        );
        kernel.apply_execution_mode(Some(ExecutionMode::Goal));
        assert_eq!(
            kernel.debug_preset().approval,
            starriver_agent_loop::approval::ApprovalMode::Auto
        );

        // Standard / None (unset) falls back to the Standard preset:
        // workspace-write + Manual (A-1 strengthened: Manual across the
        // board. Preset::default and the fallback now mean the same thing).
        kernel.apply_execution_mode(Some(ExecutionMode::Standard));
        assert_eq!(kernel.debug_preset().sandbox, SandboxMode::WorkspaceWrite);
        assert_eq!(
            kernel.debug_preset().approval,
            starriver_agent_loop::approval::ApprovalMode::Manual
        );
        kernel.apply_execution_mode(None);
        assert_eq!(kernel.debug_preset(), Preset::default());
    }

    #[tokio::test]
    async fn explicit_preset_override_survives_execution_mode_refresh() {
        use crate::execution_mode::ExecutionMode;
        use crate::preset::{Preset, MANUAL_APPROVAL};
        use starriver_kernel::sandbox::SandboxMode;

        let (bus, ctx) = bus_and_ctx();
        let persistence = temp_persistence(&bus);
        let kernel = PluginKernel::new(ctx, bus, persistence);

        let explicit = Preset {
            sandbox: SandboxMode::DangerFullAccess,
            approval: MANUAL_APPROVAL,
        };
        kernel.set_preset(explicit);
        // Execution-mode pushes no longer touch an explicitly overridden preset (tests/UI control it).
        kernel.apply_execution_mode(Some(ExecutionMode::Plan));
        assert_eq!(kernel.debug_preset(), explicit);
        kernel.apply_execution_mode(None);
        assert_eq!(kernel.debug_preset(), explicit);
    }

    #[test]
    fn assistant_reasoning_chunk_emits_thinking_delta() {
        use starriver_kernel::json::JsonValue;
        use starriver_kernel::session::SessionEvent;
        use starriver_kernel::ui::UiEvent;

        let chunk_event = |chunk: JsonValue| SessionEvent {
            seq: 1,
            time: 1,
            event_type: "assistant/chunk".into(),
            data: JsonValue::Object(vec![("chunk".into(), chunk)]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        };
        let chunk = |kind: &str, text: &str| {
            JsonValue::Object(vec![
                ("type".into(), JsonValue::String(kind.into())),
                ("text".into(), JsonValue::String(text.into())),
            ])
        };

        // reasoning-delta with non-empty text → exactly one ThinkingDelta.
        let events = bridge::translate(&chunk_event(chunk("reasoning-delta", "thinking…")), "s1");
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(
            &events[0],
            UiEvent::ThinkingDelta { text, .. } if text == "thinking…"
        ));

        // Non-reasoning blocks (text deltas) produce no events.
        assert!(bridge::translate(&chunk_event(chunk("text", "hi")), "s1").is_empty());
        // reasoning with empty text → silent.
        assert!(bridge::translate(&chunk_event(chunk("reasoning-delta", "")), "s1").is_empty());
        // Missing chunk payload → silent.
        let empty = SessionEvent {
            seq: 2,
            time: 1,
            event_type: "assistant/chunk".into(),
            data: JsonValue::Object(vec![]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        };
        assert!(bridge::translate(&empty, "s1").is_empty());
    }

    // ── Settings seam (N2): settings-gated residents ────────────────────────

    #[test]
    fn settings_residents_render_per_agent_options() {
        use starriver_kernel::context::RenderContext;
        use starriver_kernel::system_prompt::PromptSettings;

        let registry = SystemPromptRegistry::new();
        let (_bus, ctx) = bus_and_ctx();
        register_settings_residents(&registry, &ctx);
        let session = starriver_kernel::session::Session::new(
            starriver_kernel::session::SessionId("n2".into()),
            cordis_rslite::EventBus::new(),
        );
        let assemble_with = |settings: Option<PromptSettings>| {
            let options = starriver_kernel::agent::AgentOptions {
                prompt_settings: settings,
                ..Default::default()
            };
            let render = RenderContext {
                session: &session,
                options: &options,
            };
            registry.assemble(&ctx, &render).unwrap().system
        };

        // Defaults (no settings): identity on, no source, no baseline.
        let system = assemble_with(None);
        assert!(system.contains(IDENTITY_PROMPT));
        assert!(!system.contains("Source checkout:"));
        assert!(!system.contains("Platform rules"));
        // The language resident is always on: follow-the-user by default.
        assert!(
            system.contains("language of the user's messages"),
            "{system}"
        );
        // The communication resident is unconditional platform policy.
        assert!(
            system.contains("The requested scope is the deliverable"),
            "{system}"
        );

        // Full settings: identity + source + baseline, in resident order.
        let system = assemble_with(Some(PromptSettings {
            source_path: Some("/src/starriver".into()),
            persona_global: Some("Platform rules apply.".into()),
            ..Default::default()
        }));
        let identity = system.find(IDENTITY_PROMPT).expect("identity present");
        let source = system
            .find("Source checkout: /src/starriver — do not infer the working directory from this path.")
            .expect("source declaration present");
        let baseline = system
            .find("Platform rules apply.")
            .expect("baseline present");
        assert!(identity < source && source < baseline, "{system}");
        // Baseline sorts before the per-agent persona at the same order.
        assert!(
            system.find("deployment:baseline").is_none(),
            "name internal"
        );

        // Identity opt-out drops the resident entirely.
        let system = assemble_with(Some(PromptSettings {
            include_identity: false,
            ..Default::default()
        }));
        assert!(!system.contains(IDENTITY_PROMPT), "{system}");
    }

    #[test]
    fn settings_resident_helpers_render_expected_text() {
        use starriver_kernel::system_prompt::PromptSettings;

        // Identity: default-on, suppressible.
        assert_eq!(identity_section_text(None), IDENTITY_PROMPT);
        assert_eq!(
            identity_section_text(Some(&PromptSettings::default())),
            IDENTITY_PROMPT
        );
        assert_eq!(
            identity_section_text(Some(&PromptSettings {
                include_identity: false,
                ..Default::default()
            })),
            ""
        );

        // Source: only with an explicit path.
        assert_eq!(source_section_text(None), "");
        assert_eq!(
            source_section_text(Some(&PromptSettings {
                source_path: Some("/src".into()),
                ..Default::default()
            })),
            "Source checkout: /src — do not infer the working directory from this path."
        );

        // Persona global: verbatim body or empty.
        assert_eq!(persona_global_section_text(None), "");
        assert_eq!(
            persona_global_section_text(Some(&PromptSettings {
                persona_global: Some("Be faithful.".into()),
                ..Default::default()
            })),
            "Be faithful."
        );

        // Language: follow-the-user by default; a configured language pins it
        // (`language`-section parity). Blank counts as unset.
        assert_eq!(
            language_section_text(None),
            "Write user-facing replies in the language of the user's messages."
        );
        assert_eq!(
            language_section_text(Some(&PromptSettings::default())),
            "Write user-facing replies in the language of the user's messages."
        );
        assert_eq!(
            language_section_text(Some(&PromptSettings {
                response_language: Some("Chinese (中文)".into()),
                ..Default::default()
            })),
            "Write all user-facing replies and plan documents in Chinese (中文). \
             Quote code, identifiers and error output verbatim."
        );
        assert_eq!(
            language_section_text(Some(&PromptSettings {
                response_language: Some("  ".into()),
                ..Default::default()
            })),
            "Write user-facing replies in the language of the user's messages."
        );
    }

    #[test]
    fn tool_result_meta_translates_through_to_ui_event() {
        use starriver_kernel::json::JsonValue;
        use starriver_kernel::session::SessionEvent;
        let event = SessionEvent {
            seq: 4,
            time: 1,
            event_type: "tool/result".into(),
            data: JsonValue::Object(vec![
                (
                    "message".into(),
                    JsonValue::Object(vec![
                        ("role".into(), JsonValue::String("user".into())),
                        ("content".into(), JsonValue::Array(vec![])),
                        (
                            "source".into(),
                            JsonValue::Object(vec![
                                ("kind".into(), JsonValue::String("tool".into())),
                                ("callId".into(), JsonValue::String("c1".into())),
                            ]),
                        ),
                    ]),
                ),
                (
                    "meta".into(),
                    JsonValue::Object(vec![
                        ("kind".into(), JsonValue::String("todo".into())),
                        ("total".into(), JsonValue::UInt(3)),
                    ]),
                ),
            ]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        };
        let events = bridge::translate(&event, "s1");
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            UiEvent::ToolResult { meta, ok, .. } => {
                assert!(*ok);
                let meta = meta.as_ref().expect("meta forwarded");
                assert_eq!(meta["kind"], "todo");
                assert_eq!(meta["total"], 3);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }

        // No meta in the event → None (skip_serializing_if keeps the wire lean).
        let mut bare = event;
        if let JsonValue::Object(pairs) = &mut bare.data {
            pairs.retain(|(k, _)| k != "meta");
        }
        let events = bridge::translate(&bare, "s1");
        assert!(matches!(&events[0], UiEvent::ToolResult { meta: None, .. }));
    }

    #[test]
    fn coding_preset_reads_hot_settings() {
        let mut settings = crate::settings::Settings::default();
        settings.agent.max_parallel_tool_calls = Some(3);
        settings.agent.context_window = Some(4096);
        settings.agent.time_refresh_interval_ms = Some(30_000);
        settings.agent.turn_tool_call_budget = Some(200);
        let preset = coding_preset(&settings);
        assert_eq!(preset.max_parallel_tool_calls, Some(3));
        assert_eq!(preset.context_window, Some(4096));
        assert_eq!(preset.time_refresh_interval_ms, Some(30_000));
        assert_eq!(preset.turn_tool_call_budget, Some(200));
        // Defaults: no settings override → time-context falls back to 60s,
        // parallel budget stays None (loop default 10), turn budget stays
        // None (loop default 666).
        let defaults = coding_preset(&crate::settings::Settings::default());
        assert_eq!(defaults.time_refresh_interval_ms, Some(60_000));
        assert_eq!(defaults.max_parallel_tool_calls, None);
        assert_eq!(defaults.turn_tool_call_budget, None);
    }

    #[test]
    fn compaction_summary_event_translates_to_compaction_ui_event() {
        use starriver_kernel::json::JsonValue;
        use starriver_kernel::session::SessionEvent;
        let event = SessionEvent {
            seq: 9,
            time: 1,
            event_type: "compaction/summary".into(),
            data: JsonValue::Object(vec![
                ("shadowedStart".into(), JsonValue::UInt(0)),
                ("shadowedEnd".into(), JsonValue::UInt(7)),
                (
                    "summary".into(),
                    JsonValue::String("## Primary Request\n(none)".into()),
                ),
                ("provider".into(), JsonValue::String("mock".into())),
                ("model".into(), JsonValue::String("m1".into())),
            ]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        };
        let events = bridge::translate(&event, "s1");
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(
            &events[0],
            UiEvent::Compaction {
                summary,
                shadowed_start: 0,
                shadowed_end: 7,
                ..
            } if summary.contains("Primary Request")
        ));

        // Empty summary (failed summarize) → no UI event; bookkeeping events
        // (start/end) never surface.
        let mut silent = event.clone();
        silent.event_type = "compaction/start".into();
        assert!(bridge::translate(&silent, "s1").is_empty());
        silent.event_type = "compaction/end".into();
        assert!(bridge::translate(&silent, "s1").is_empty());
        let mut empty = event;
        if let JsonValue::Object(pairs) = &mut empty.data {
            pairs.retain(|(k, _)| k != "summary");
        }
        assert!(bridge::translate(&empty, "s1").is_empty());
    }

    #[test]
    fn tool_call_translate_carries_object_arguments_as_json_text() {
        // WI-47 Phase B dual-format: new logs store `arguments` as the
        // lossless object — the UiEvent must carry it as JSON text so the UI
        // renders the actual command (`read: path`), not the bare tool name.
        let event = starriver_kernel::session::SessionEvent {
            seq: 1,
            time: 0,
            event_type: "tool/call".into(),
            data: starriver_kernel::json::JsonValue::Object(vec![
                (
                    "callId".into(),
                    starriver_kernel::json::JsonValue::String("c1".into()),
                ),
                (
                    "name".into(),
                    starriver_kernel::json::JsonValue::String("read".into()),
                ),
                (
                    "arguments".into(),
                    starriver_kernel::json::JsonValue::Object(vec![(
                        "path".into(),
                        starriver_kernel::json::JsonValue::String("/tmp/a.md".into()),
                    )]),
                ),
            ]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        };
        let out = bridge::translate(&event, "s1");
        match &out[0] {
            UiEvent::ToolCall { name, input, .. } => {
                assert_eq!(name, "read");
                let input = input.as_deref().expect("object arguments surface as input");
                assert!(input.contains("/tmp/a.md"), "{input}");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }

        // Old logs: string arguments still ride through verbatim.
        let string_args = starriver_kernel::session::SessionEvent {
            event_type: "tool/call".into(),
            data: starriver_kernel::json::JsonValue::Object(vec![
                (
                    "callId".into(),
                    starriver_kernel::json::JsonValue::String("c2".into()),
                ),
                (
                    "name".into(),
                    starriver_kernel::json::JsonValue::String("bash".into()),
                ),
                (
                    "arguments".into(),
                    starriver_kernel::json::JsonValue::String(r#"{"command":"ls"}"#.into()),
                ),
            ]),
            ..event
        };
        let out = bridge::translate(&string_args, "s1");
        match &out[0] {
            UiEvent::ToolCall {
                input: Some(input), ..
            } => {
                assert!(input.contains("ls"), "{input}");
            }
            other => panic!("expected ToolCall with input, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn todo_presentation_meta_counts_status_completed() {
        let (bus, ctx) = bus_and_ctx();
        let persistence = temp_persistence(&bus);
        let kernel = PluginKernel::new(ctx, bus, persistence);
        let input = starriver_kernel::tools::ToolExecutionInput {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: None,
            name: "todo_write".into(),
            arguments: {
                use starriver_kernel::json::JsonValue;
                cordis_rslite::Value::new(JsonValue::Object(vec![(
                    "items".into(),
                    JsonValue::Array(vec![
                        JsonValue::Object(vec![
                            ("title".into(), JsonValue::String("a".into())),
                            ("checked".into(), JsonValue::Bool(true)),
                        ]),
                        JsonValue::Object(vec![
                            ("title".into(), JsonValue::String("b".into())),
                            ("status".into(), JsonValue::String("completed".into())),
                        ]),
                        JsonValue::Object(vec![
                            ("title".into(), JsonValue::String("c".into())),
                            ("status".into(), JsonValue::String("pending".into())),
                        ]),
                    ]),
                )]))
            },
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            cancellation: None,
        };
        let meta = starriver_kernel::tools::ToolRuntime::presentation_meta(
            kernel.tools(),
            &input,
            &cordis_rslite::Value::new(()),
        )
        .expect("todo_write meta present");
        assert_eq!(meta["total"], 3);
        assert_eq!(
            meta["completed"], 2,
            "checked:true and status:completed both count as done"
        );
    }
}
