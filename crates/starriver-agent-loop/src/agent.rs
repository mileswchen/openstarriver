//! Concrete loop driver: one agent, its durable inbox, and the turn/step
//! state machine.

use starriver_compaction::CompactionTrigger;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context as TaskContext, Poll};

use cordis_rslite::{BoxFuture, Context, EventError};
use futures::StreamExt;
use starriver_kernel::agent::{
    with_initiator, Agent, AgentCancelCause, AgentErrorEvent, AgentInboxClaimed,
    AgentInboxDiscarded, AgentInboxInserted, AgentOptions, AgentPreStep, AgentRequest,
    AgentRequestError, AgentStatus, AgentStatusChanged, AgentTurnStopping, Inbox,
    InboxNotifications, InboxTarget, PendingMessages, PreStepDecision, RequestErrorAction,
};
use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::{
    mint_message_id, BlockAssembler, ContentBlock, FinishReason, GenerateOptions, LlmCallConfig,
    LlmCallConfigAdapterDefaults, LlmError, LlmFailure, Message, MessageId, MessageRole,
    MessageSource, PreparedLlmCall, RetryBackoff, RetryPolicy, StreamChunk, TokenUsage, ToolSchema,
};
use starriver_kernel::session::{AppendOptions, Session, SessionId, SurfaceOp, TurnEndReason};
use starriver_kernel::tools::ToolError;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::services::{LlmRuntimeView, ToolRuntimeView};

/// Merge static prompt sections into one system string. Empty sections fall
/// back to the legacy whole-string `system`; otherwise sections are sorted by
/// name and joined. Aligns with prompt sections (variables/runtime
/// context are a later phase).
fn assemble_system(options: &AgentOptions) -> Option<String> {
    if options.system_sections.is_empty() {
        return options.system.clone();
    }
    let mut sections = options.system_sections.clone();
    sections.sort_by(|a, b| a.0.cmp(&b.0));
    Some(
        sections
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

/// Render the full dynamic context snapshot text. Empty sections yield an
/// empty string (projection turns that into the CLEARED marker). The sections
/// come from `SystemPromptRegistry::assemble().contexts` — the runtime-context
/// face of one pre-step assembly.
fn render_snapshot(sections: &[starriver_kernel::context::AssembledContext]) -> String {
    let body = sections
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    if body.is_empty() {
        String::new()
    } else {
        format!(
            "Current runtime context. This snapshot supersedes earlier runtime-context snapshots.\n\n{body}"
        )
    }
}

fn prompt_failure(error: &starriver_kernel::system_prompt::AssembleError) -> LlmFailure {
    LlmFailure {
        message: error.to_string(),
        code: "PROMPT_ASSEMBLE".into(),
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    }
}

enum Phase {
    Idle {
        last_turn: u64,
    },
    Running {
        abort: CancellationToken,
        turn: u64,
        step: u64,
    },
}

struct LoopState {
    /// Level-triggered wake: one permit per waking send. Permits persist, so
    /// a wake that arrives before the driver task starts is not lost. The
    /// driver CONSUMES each permit (`forget`) — a recycled permit would
    /// livelock the outer loop (BUG-106).
    wake: Semaphore,
    /// Fires when a driver run reaches convergence (no pending work).
    run_done: Notify,
    /// Whether a driver run is currently active.
    running: AtomicBool,
    /// Whether the persistent task has been started.
    started: AtomicBool,
    /// Set by `cancel(keep_inbox = true)`: pending inbox is kept but the
    /// driver must not auto-continue into it (manual stop parks the queue).
    /// Cleared by the next waking `send` / `pending_flush`.
    parked: AtomicBool,
}

/// In-memory request header identity for change detection.
#[derive(Clone, PartialEq)]
struct HeaderKey {
    config: LlmCallConfig,
    system: Option<String>,
    tool_names: Vec<String>,
}

/// One assembled model request plus its prepared call, if any.
struct AssembledRequest {
    request: GenerateOptions,
    prepared: Option<PreparedLlmCall>,
}

/// Polls `F` with per-poll panic containment. On panic the driver run is
/// abandoned and reported instead of killing the persistent task.
struct CatchPanic<F> {
    inner: F,
}

impl<F: Future> Future for CatchPanic<F> {
    type Output = Result<F::Output, String>;
    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        // SAFETY: the inner future is only moved when the wrapper is dropped,
        // never out of the pinned wrapper.
        let inner = unsafe { self.map_unchecked_mut(|this| &mut this.inner) };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || inner.poll(cx)));
        match result {
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(panic_message(payload))),
        }
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "agent driver panicked".into()
    }
}

/// Concrete driver implementing the kernel `Agent` handle.
pub struct ReactLoopAgent {
    loop_ctx: Context,
    id: SessionId,
    options: AgentOptions,
    session: Session,
    agent_ctx: Context,
    llm: Arc<dyn LlmRuntimeView>,
    tools: Arc<dyn ToolRuntimeView>,
    /// Shared system-prompt registry (identity/persona/skills/review residents
    /// plus runtime-context contributions). Passed as a concrete `Arc` — it is
    /// Clone and internally locked; no view trait needed.
    system_prompts: Arc<starriver_kernel::system_prompt::SystemPromptRegistry>,
    runtime_context: Arc<std::sync::Mutex<crate::context::RuntimeContextProjection>>,
    /// Workspace-instructions change-only projector (SP3): digest-deduped
    /// recomposition of the discovery set at every pre-step.
    instructions: std::sync::Mutex<crate::instructions::InstructionsProjection>,
    /// Time-context sampler (SP4): throttled by the agent options.
    time_context: std::sync::Mutex<crate::time_context::TimeProjection>,
    /// MCP availability notice (WI-163): change-only projector over the
    /// host-fed unavailable-server snapshot.
    mcp_status: std::sync::Mutex<crate::mcp_status::McpStatusProjection>,
    inbox: Mutex<Inbox>,
    phase: Mutex<Phase>,
    last_cancel: Mutex<Option<AgentCancelCause>>,
    state: Arc<LoopState>,
    me: Mutex<Weak<ReactLoopAgent>>,
    header_logged: AtomicBool,
    last_header: Mutex<Option<HeaderKey>>,
    last_proposal: Mutex<Option<LlmCallConfig>>,
    last_adapter_defaults: Mutex<LlmCallConfigAdapterDefaults>,
    /// WI-1: file ids pulled hot by `reload_image` calls this turn; cleared
    /// at turn/end so a pull lasts exactly one turn. BUG-115/WI-160: only
    /// ids whose tool result settled WITHOUT an error get pinned here
    /// (confirmation scan below) — a NOT_FOUND attempt must not pin.
    reload_ids: Mutex<std::collections::HashSet<String>>,
    /// WI-160: every file_id a reload_image call named this turn, executed
    /// or not — the duplicate short-circuit keys on this set, so a repeat
    /// of a FAILED id is answered with a failure-aware stop hint instead
    /// of a lying "already re-attached".
    attempted_reload_ids: Mutex<std::collections::HashSet<String>>,
    /// WI-160: session-event seq already visited by the reload confirmation
    /// scan (monotonic across turns — the scan only ever moves forward).
    reload_confirm_cursor: Mutex<u64>,
    /// BUG-135: per-turn reload_image storm guard. Either budget (same-id
    /// noop repeats, turn-total reload calls) tripping the breaker
    /// force-ends the turn — hint-immune models would otherwise loop noops
    /// forever.
    reload_storm: Mutex<crate::tool_calls::ReloadStorm>,
    /// BUG-138: per-turn repeat guard for every NON-reload tool — consecutive
    /// identical-call failures and the turn's total non-reload calls. The
    /// generalized version of the BUG-135 defense line: any tool stuck in the
    /// unavailable/failing/pattern-locked shape gets the turn terminated
    /// instead of looping ~7s/step forever.
    turn_repeat: Mutex<crate::tool_calls::TurnRepeatGuard>,
}

/// Bridge from the loop's LLM runtime view to the compaction crate's
/// summarize port: folds the streamed reply into its text (WI-9).
struct LoopCompactionLlm {
    llm: Arc<dyn LlmRuntimeView>,
}

impl starriver_compaction::CompactionLlm for LoopCompactionLlm {
    fn summarize(
        &self,
        options: starriver_kernel::llm::GenerateOptions,
    ) -> futures::future::BoxFuture<'static, Result<String, String>> {
        let mut stream = self.llm.stream(options);
        Box::pin(async move {
            let mut summary = String::new();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(StreamChunk::BlockEnd {
                        block: ContentBlock::Text(t),
                        ..
                    }) => summary.push_str(&t),
                    Err(e) => return Err(e.to_string()),
                    _ => {}
                }
            }
            Ok(summary)
        })
    }
}

impl ReactLoopAgent {
    /// Persist one projected snapshot as a surface user message. Tolerant of
    /// serialization failures — a projection that cannot serialize is dropped
    /// rather than breaking the turn.
    fn append_snapshot_message(&self, message: Message) {
        let Ok(data) = message.to_json() else { return };
        let _ = self.session.append_with(
            "user/message",
            data,
            starriver_kernel::session::AppendOptions {
                surface_op: Some(starriver_kernel::session::SurfaceOp::Append),
                source_event_seqs: Vec::new(),
            },
        );
    }

    /// Compact the context when the derived surface's estimated token count
    /// exceeds `threshold × context_window`. The older messages (everything
    /// but the retained tail, never splitting a tool-call pair) are summarized
    /// by an auxiliary call that reuses the step's assembled system prompt and
 /// the live surface as its prefix (summarizer parity — providers with
    /// prompt caching re-emit only the trailing instruction), and a replace
    /// checkpoint shadows exactly the summarized seq range; the retained tail
    /// stays verbatim on the surface. compaction/* log-only events record the
    /// attempt for consumers without entering the surface.
 /// WI-9: retries after a confirmed context-window overflow (
    /// maxOverflowRetries, default 1; 0 disables recovery).
    fn max_overflow_retries(&self) -> u32 {
        self.options
            .compaction_tuning
            .as_ref()
            .and_then(|t| t.max_overflow_retries)
            .unwrap_or(1)
    }

    async fn compact_if_needed(&self, system: Option<&str>, window: u64, threshold: f64) {
        self.compact_inner(system, window, threshold, CompactionTrigger::Pressure)
            .await;
    }

    /// Manual compaction trigger (`session.compact`): the same routine as the
    /// per-turn check with the threshold bypassed (threshold 0 ⇒ always due).
    /// Returns (before, after) token estimates when compaction ran.
    pub async fn force_compact(&self, system: Option<&str>, window: u64) -> Option<(u64, u64)> {
        self.compact_inner(system, window, 0.0, CompactionTrigger::Manual)
            .await
    }

}

impl ReactLoopAgent {
    async fn force_compact_keep_recent_inner(
        &self,
        system: Option<&str>,
        window: u64,
        keep_recent: u32,
    ) -> Option<(u64, u64)> {
        let surface_len = self.session.surface().nodes.len().max(1) as f64;
        let ratio = (keep_recent as f64 / surface_len).clamp(0.0, 1.0);
        self.compact_inner_with_retain(system, window, 0.0, ratio, CompactionTrigger::Manual)
            .await
    }

    /// Overflow-recovery compaction (`agent.max_overflow_retries`): the
    /// threshold comparison is bypassed — a confirmed context-window overflow
    /// condenses unconditionally.
    async fn compact_for_overflow(&self, system: Option<&str>, window: u64) -> Option<(u64, u64)> {
        self.compact_inner(system, window, 0.0, CompactionTrigger::ContextOverflow)
            .await
    }

    /// Thin bridge: build the crate-side policy/route from agent options and
    /// delegate to `starriver_compaction::compact`. `None` = nothing
    /// compacted (below pressure, guards, or failed summarize — the surface
    /// is left unchanged either way).
    async fn compact_inner(
        &self,
        system: Option<&str>,
        window: u64,
        threshold: f64,
        trigger: CompactionTrigger,
    ) -> Option<(u64, u64)> {
        let retain = self
            .options
            .compaction_retain_ratio
            .unwrap_or(starriver_compaction::DEFAULT_RETAIN_RATIO);
        self.compact_inner_with_retain(system, window, threshold, retain, trigger)
            .await
    }

    async fn compact_inner_with_retain(
        &self,
        system: Option<&str>,
        window: u64,
        threshold: f64,
        retain_ratio: f64,
        trigger: CompactionTrigger,
    ) -> Option<(u64, u64)> {
        let provider = self.options.provider.clone().unwrap_or_default();
        let model = self.options.model.clone().unwrap_or_default();
        if provider.is_empty() || model.is_empty() {
            return None;
        }
        let policy = starriver_compaction::CompactionPolicy::from_tuning(
            threshold,
            retain_ratio,
            self.options.compaction_tuning.as_ref(),
        );
        let route = starriver_compaction::CompactionRoute {
            provider,
            model,
            system: system.map(String::from),
            session_id: self.id.0.clone(),
            context_window: window,
            max_tokens: 1024,
        };
        let llm = LoopCompactionLlm {
            llm: Arc::clone(&self.llm),
        };
        starriver_compaction::compact(&self.session, &llm, route, policy, trigger).await
    }

    pub fn new(
        loop_ctx: Context,
        id: SessionId,
        options: AgentOptions,
        session: Session,
        llm: Arc<dyn LlmRuntimeView>,
        tools: Arc<dyn ToolRuntimeView>,
        system_prompts: Arc<starriver_kernel::system_prompt::SystemPromptRegistry>,
    ) -> Arc<Self> {
        let tag: Arc<str> = Arc::from(id.0.as_str());
        let expected = tag.clone();
        let agent_ctx = loop_ctx
            .tagged(tag)
            .with_filter(move |owner| owner.tag().map(|t| t.as_ref()) == Some(expected.as_ref()));
 // Builtin variables (agent-loop parity): the route pair and the
        // frozen workspace-root header value. Scoped to this agent's tag;
        // unknown `{{refs}}` elsewhere still error at assembly.
        //
        // {{provider}}/{{model}} read the CURRENT options at every assembly
        // (WI-49): a /model switch re-publishes the agent with fresh options
        // on the next execute, and the next step's assembled system carries
        // the new route pair without re-registering anything — even when the
        // scoped layer still holds a previous publish's registration (same
        // tag → duplicate registration is a no-op, and every registration is
        // now the same live reader of the assembling agent's options).
        // {{cwd}} stays a frozen clone: it mirrors the immutable session
        // header (prompt-parity D1 — cwd is a SYSTEM variable sourced only
        // from the header, never re-derived).
        let _provider_handle = system_prompts.variable(
            &agent_ctx,
            starriver_kernel::system_prompt::VariableEntry {
                name: "provider".into(),
                provider: Arc::new(|render| render.options.provider.clone().unwrap_or_default()),
            },
        );
        let _model_handle = system_prompts.variable(
            &agent_ctx,
            starriver_kernel::system_prompt::VariableEntry {
                name: "model".into(),
                provider: Arc::new(|render| render.options.model.clone().unwrap_or_default()),
            },
        );
        let cwd = session.header.cwd.clone().unwrap_or_default();
        let _cwd_handle = system_prompts.variable(
            &agent_ctx,
            starriver_kernel::system_prompt::VariableEntry {
                name: "cwd".into(),
                provider: Arc::new(move |_| cwd.clone()),
            },
        );
        let last_turn = initial_last_turn(&session);
        let bus = loop_ctx.event_bus().cloned();
        let cell = Arc::new(Mutex::new(None::<Weak<ReactLoopAgent>>));
        let notifications = InboxNotifications {
            inserted: {
                let cell = Arc::clone(&cell);
                let bus = bus.clone();
                Arc::new(move |message| {
                    let Some(me) = cell.lock().unwrap().as_ref().and_then(Weak::upgrade) else {
                        return;
                    };
                    if let Some(bus) = &bus {
                        bus.emit_scoped(
                            &me.agent_ctx,
                            Arc::new(AgentInboxInserted {
                                agent: me.clone() as Arc<dyn Agent>,
                                message,
                            }),
                        );
                    }
                })
            },
            discarded: {
                let cell = Arc::clone(&cell);
                let bus = bus.clone();
                Arc::new(move |message| {
                    let Some(me) = cell.lock().unwrap().as_ref().and_then(Weak::upgrade) else {
                        return;
                    };
                    if let Some(bus) = &bus {
                        bus.emit_scoped(
                            &me.agent_ctx,
                            Arc::new(AgentInboxDiscarded {
                                agent: me.clone() as Arc<dyn Agent>,
                                message,
                            }),
                        );
                    }
                })
            },
            claimed: {
                let cell = Arc::clone(&cell);
                let bus = bus.clone();
                Arc::new(move |message, turn| {
                    let Some(me) = cell.lock().unwrap().as_ref().and_then(Weak::upgrade) else {
                        return;
                    };
                    if let Some(bus) = &bus {
                        bus.emit_scoped(
                            &me.agent_ctx,
                            Arc::new(AgentInboxClaimed {
                                agent: me.clone() as Arc<dyn Agent>,
                                message,
                                turn,
                            }),
                        );
                    }
                })
            },
        };
        let inbox = Inbox::new(session.clone(), notifications);
        let runtime_context = Arc::new(std::sync::Mutex::new(
            crate::context::RuntimeContextProjection::new(&agent_ctx, &session),
        ));
        let instructions =
            std::sync::Mutex::new(crate::instructions::InstructionsProjection::with_budget(
                &session,
                options
                    .prompt_settings
                    .as_ref()
                    .and_then(|settings| settings.instructions_max_bytes),
            ));
        // Settings namespace (LLP): suppress runtime-context snapshots when
        // the composition opts out.
        if let Some(ref settings) = options.prompt_settings {
            if !settings.include_runtime_context {
                system_prompts.suppress_runtime_context(&agent_ctx).forget();
            }
        }
        let time_context =
            std::sync::Mutex::new(crate::time_context::TimeProjection::new(&session));
        let mcp_status =
            std::sync::Mutex::new(crate::mcp_status::McpStatusProjection::new(&session));
        let agent = Arc::new(Self {
            loop_ctx,
            id,
            options,
            session,
            agent_ctx,
            llm,
            tools,
            system_prompts,
            runtime_context,
            instructions,
            time_context,
            mcp_status,
            inbox: Mutex::new(inbox),
            phase: Mutex::new(Phase::Idle { last_turn }),
            last_cancel: Mutex::new(None),
            state: Arc::new(LoopState {
                wake: Semaphore::new(0),
                run_done: Notify::new(),
                running: AtomicBool::new(false),
                started: AtomicBool::new(false),
                parked: AtomicBool::new(false),
            }),
            me: Mutex::new(Weak::new()),
            header_logged: AtomicBool::new(false),
            last_header: Mutex::new(None),
            last_proposal: Mutex::new(None),
            last_adapter_defaults: Mutex::new(LlmCallConfigAdapterDefaults::default()),
            reload_ids: Mutex::new(std::collections::HashSet::new()),
            attempted_reload_ids: Mutex::new(std::collections::HashSet::new()),
            reload_confirm_cursor: Mutex::new(0),
            reload_storm: Mutex::new(crate::tool_calls::ReloadStorm::default()),
            turn_repeat: Mutex::new(crate::tool_calls::TurnRepeatGuard::default()),
        });
        *cell.lock().unwrap() = Some(Arc::downgrade(&agent));
        *agent.me.lock().unwrap() = Arc::downgrade(&agent);
        agent
    }

    /// Start the persistent driver task. Idempotent; called after publication.
    pub fn start(self: &Arc<Self>) {
        if self.state.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                // BUG-106: the wake permit must be consumed, never recycled.
                // Dropping an `acquire()` guard returns the permit to the
                // semaphore, so binding the guard to one loop iteration
                // re-arms the loop forever (acquire → nothing pending →
                // drop/return → acquire …) at full core speed once any
                // iteration completes. `forget()` keeps the level-triggered
                // contract: every waking `send()` powers exactly one outer
                // iteration, and an idle driver always parks at zero
                // permits.
                me.state
                    .wake
                    .acquire()
                    .await
                    .expect("agent wake semaphore closed")
                    .forget();
                me.state.running.store(true, Ordering::SeqCst);
                loop {
                    let should_run = {
                        let pending = me.inbox_lock().has_pending();
                        let idle = matches!(&*me.phase.lock().unwrap(), Phase::Idle { .. });
                        let parked = me.state.parked.load(Ordering::SeqCst);
                        pending && idle && !parked
                    };
                    if !should_run {
                        break;
                    }
                    {
                        let mut phase = me.phase.lock().unwrap();
                        let Phase::Idle { last_turn } = &*phase else {
                            break;
                        };
                        *phase = Phase::Running {
                            abort: CancellationToken::new(),
                            turn: *last_turn,
                            step: 0,
                        };
                    }
                    me.emit_agent_status(AgentStatus::Running);
                    let driver: Arc<dyn Agent> = me.clone();
                    let result = CatchPanic {
                        inner: with_initiator(driver, me.clone().drive()),
                    }
                    .await;
                    {
                        let mut phase = me.phase.lock().unwrap();
                        let last_turn = match &*phase {
                            Phase::Running { turn, .. } => *turn,
                            Phase::Idle { last_turn } => *last_turn,
                        };
                        *phase = Phase::Idle { last_turn };
                    }
                    me.emit_agent_status(AgentStatus::Idle);
                    if let Err(message) = result {
                        tracing::error!(
                            agent = %me.id.0,
                            error = %message,
                            "agent driver panicked"
                        );
                        me.emit_agent_error(
                            0,
                            0,
                            &LlmFailure {
                                message,
                                code: "DRIVER_PANIC".into(),
                                status: None,
                                provider_retry_after_ms: None,
                                request_id: None,
                            },
                        );
                        break;
                    }
                }
                me.state.running.store(false, Ordering::SeqCst);
                // notify_one stores a permit when no waiter is present, so a
                // when_idle caller that registers after the run finishes is
                // still woken.
                me.state.run_done.notify_one();
            }
        });
    }

    /// One driver run: open turns until no pending work remains.
    async fn drive(self: Arc<Self>) {
        while !self.state.parked.load(Ordering::SeqCst) && self.turn().await {}
    }

    /// Open and run one turn. Returns whether pending work remains.
    async fn turn(&self) -> bool {
        let (turn, abort) = {
            let mut phase = self.phase.lock().unwrap();
            let Phase::Running {
                turn, step, abort, ..
            } = &mut *phase
            else {
                return false;
            };
            *turn += 1;
            *step = 0;
 // Each turn gets a fresh cancellation signal, matching
            // per-turn AbortController: an abort only affects the turn it was
            // issued in, and a later queued turn starts clean.
            *abort = CancellationToken::new();
            (*turn, abort.clone())
        };
        self.session
            .append_with(
                "turn/start",
                JsonValue::Object(vec![("turn".into(), JsonValue::UInt(turn))]),
                AppendOptions::default(),
            )
            .expect("turn/start append failed");

        #[allow(unused_assignments)] // the initializer satisfies the type system.
        let mut turn_end: Option<TurnEndReason> = None;
        let mut max_tokens_hit = false;
        let mut target = InboxTarget::NextTurn;
        loop {
            // Per-step assembly output: the system string stays constant for
 // every request inside this step an empty registry
            // falls back to the legacy whole-string / flat-section options.
            let mut step_system: Option<String> = None;
            if abort.is_cancelled() {
                turn_end = Some(TurnEndReason::Aborted {
                    cause: self.take_cancel_cause(),
                });
                break;
            }
            let step = {
                let mut phase = self.phase.lock().unwrap();
                let Phase::Running { step, .. } = &mut *phase else {
                    unreachable!("running phase")
                };
                *step += 1;
                *step
            };
            let claimed = self.inbox_lock().claim(target, turn);
            let decision = match self.pre_step(&abort, turn, step, claimed).await {
                Ok(decision) => decision,
                Err(error) => {
                    turn_end = Some(TurnEndReason::Error {
                        failure: event_failure(&error),
                    });
                    break;
                }
            };
            if abort.is_cancelled() {
                turn_end = Some(TurnEndReason::Aborted {
                    cause: self.take_cancel_cause(),
                });
                break;
            }
            let mut pending_sections: Vec<starriver_kernel::context::AssembledContext> = Vec::new();
            if matches!(decision, PreStepDecision::Enter { .. }) {
                let render = starriver_kernel::context::RenderContext {
                    session: &self.session,
                    options: &self.options,
                };
 // toolOrder parity: `prompt.tool_order` drives the
                // `<tool>:guide` presentation order. An invalid configured
                // order must never kill a run — log once and fall back to the
                // default (no-hint) assembly. Write-time validation in the
                // host settings patch is the primary guard.
                let hint = render
                    .options
                    .prompt_settings
                    .as_ref()
                    .and_then(|ps| ps.tool_order.as_ref())
                    .map(|order| starriver_kernel::system_prompt::ToolOrderHint {
                        order: order.clone(),
                    });
                let assembled = match self
                    .system_prompts
                    .assemble_with_tool_order(&self.agent_ctx, &render, hint.as_ref())
                {
                    Ok(assembly) => Ok(assembly),
                    Err(error) if hint.is_some() => {
                        tracing::warn!(error = %error, "prompt.tool_order rejected — falling back to default guide order");
                        self.system_prompts.assemble(&self.agent_ctx, &render)
                    }
                    Err(error) => Err(error),
                };
                match assembled {
                    Ok(assembly) => {
                        step_system = if assembly.system.is_empty() {
                            assemble_system(&self.options)
                        } else {
                            Some(assembly.system.clone())
                        };
                        pending_sections = assembly.contexts.into_iter().map(Into::into).collect();
                    }
                    Err(error) => {
                        turn_end = Some(TurnEndReason::Error {
                            failure: prompt_failure(&error),
                        });
                        break;
                    }
                }
            }
            match decision {
                PreStepDecision::Reject => {
                    turn_end = Some(TurnEndReason::Blocked);
                    break;
                }
                PreStepDecision::Enter { messages } => {
                    if step == 1 && messages.is_empty() {
                        turn_end = Some(TurnEndReason::Completed);
                        break;
                    }
                    self.session
                        .append_with(
                            "step/start",
                            JsonValue::Object(vec![
                                ("turn".into(), JsonValue::UInt(turn)),
                                ("step".into(), JsonValue::UInt(step)),
                            ]),
                            AppendOptions::default(),
                        )
                        .expect("step/start append failed");
                    // Fixed pre-step order (prompt-parity §2.4): claimed user
                    // input → agent-instructions → runtime-context.
                    for message in messages {
                        self.session
                            .append_with(
                                "user/message",
                                message.to_json().expect("message json"),
                                AppendOptions {
                                    surface_op: Some(SurfaceOp::Append),
                                    source_event_seqs: Vec::new(),
                                },
                            )
                            .expect("user/message append failed");
                    }
                    // Workspace instructions recompute every pre-step; the
                    // digest-deduped projection only fires on change.
                    if let Some(root) = self.session.header.cwd.as_deref() {
                        let projection = self
                            .instructions
                            .lock()
                            .unwrap()
                            .project(std::path::Path::new(root));
                        if let Some(message) = projection {
                            self.append_snapshot_message(message);
                        }
                    }
                    {
                        let current = render_snapshot(&pending_sections);
                        let projection = self
                            .runtime_context
                            .lock()
                            .unwrap()
                            .project(&current, pending_sections);
                        if let Some(message) = projection {
                            self.append_snapshot_message(message);
                        }
                    }
                    // WI-163: MCP availability notice — change-only projection
                    // over the host-fed snapshot of unregistered servers.
                    // Silent capability loss had models plan around tools
                    // that no longer existed.
                    if let Some(feed) = self.options.mcp_status_feed.as_ref() {
                        let unavailable = (feed.0)();
                        let projection = self
                            .mcp_status
                            .lock()
                            .unwrap()
                            .project(&unavailable);
                        if let Some(message) = projection {
                            self.append_snapshot_message(message);
                        }
                    }
                    // 4) time-context (only when the options enable it).
                    if self.options.time_refresh_interval_ms.is_some() {
                        let now_ms = std::time::SystemTime::now()                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let preceding = self
                            .session
                            .events()
                            .iter()
                            .rev()
                            .find(|e| {
                                matches!(
                                    e.event_type.as_str(),
                                    "user/message" | "assistant/message" | "tool/result"
                                )
                            })
                            .map(|e| e.time);
                        let projection = self.time_context.lock().unwrap().project(
                            now_ms,
                            turn,
                            step,
                            self.options.time_refresh_interval_ms,
                            preceding,
                        );
                        if let Some(message) = projection {
                            self.append_snapshot_message(message);
                        }
                    }

                    // ── Compaction (SP-c): reduce context when over threshold ──
                    if let Some(window) = self.options.context_window {
                        let threshold = self
                            .options
                            .compaction_threshold_ratio
                            .unwrap_or(starriver_compaction::DEFAULT_THRESHOLD_RATIO);
                        self.compact_if_needed(step_system.as_deref(), window, threshold)
                            .await;
                    }

                    let step_end = self.step(&abort, turn, step, step_system.as_deref()).await;
                    if matches!(step_end, Some(TurnEndReason::MaxTokens)) {
                        // Max-tokens is sticky: later steps that complete
                        // normally must not downgrade the turn outcome.
                        max_tokens_hit = true;
                    }
                    self.session
                        .append_with(
                            "step/end",
                            JsonValue::Object(vec![
                                ("turn".into(), JsonValue::UInt(turn)),
                                ("step".into(), JsonValue::UInt(step)),
                            ]),
                            AppendOptions::default(),
                        )
                        .expect("step/end append failed");
                    if (max_tokens_hit || step_end.is_some())
                        && self.inbox_lock().next_step().is_empty()
                    {
 // Serial pre-close hook, matching
                        // `agent/turn-stopping`: listeners may inject next-step
                        // input to keep the turn running.
                        self.turn_stopping(turn, &abort).await;
                        if abort.is_cancelled() {
                            // Preserve the step's aborted cause; only a fresh
                            // abort consumes a new one.
                            turn_end = match step_end {
                                Some(reason @ TurnEndReason::Aborted { .. }) => Some(reason),
                                _ => Some(TurnEndReason::Aborted {
                                    cause: self.take_cancel_cause(),
                                }),
                            };
                            break;
                        }
                        if self.inbox_lock().next_step().is_empty() {
                            // Stickiness scopes to steps that complete
                            // NORMALLY: an Error (or Aborted) from a later
                            // step must surface as itself — rebranding it as
                            // MaxTokens swallowed the failure (no error
                            // event, wrong turn/end reason).
                            turn_end = match step_end {
                                Some(
                                    reason @ (TurnEndReason::Error { .. }
                                    | TurnEndReason::Aborted { .. }),
                                ) => Some(reason),
                                _ if max_tokens_hit => Some(TurnEndReason::MaxTokens),
                                _ => step_end,
                            };
                            break;
                        }
                    }
                    target = InboxTarget::NextStep;
                }
            }
        }
        if let Some(TurnEndReason::Error { failure }) = &turn_end {
            let step = match &*self.phase.lock().unwrap() {
                Phase::Running { step, .. } => *step,
                Phase::Idle { .. } => 0,
            };
            self.emit_agent_error(turn, step, failure);
        }
        self.session
            .append_with(
                "turn/end",
                JsonValue::Object(vec![
                    ("turn".into(), JsonValue::UInt(turn)),
                    (
                        "reason".into(),
                        turn_end
                            .clone()
                            .unwrap_or(TurnEndReason::Completed)
                            .to_json(),
                    ),
                ]),
                AppendOptions::default(),
            )
            .expect("turn/end append failed");
        // WI-1: reload pulls are turn-scoped — a fresh turn starts cold again.
        self.reload_ids.lock().unwrap().clear();
        self.attempted_reload_ids.lock().unwrap().clear();
        *self.reload_storm.lock().unwrap() = crate::tool_calls::ReloadStorm::default();
        // BUG-138: the general repeat guard is turn-scoped too.
        *self.turn_repeat.lock().unwrap() = crate::tool_calls::TurnRepeatGuard::default();
        self.inbox_lock().has_pending()
    }

    async fn pre_step(
        &self,
        abort: &CancellationToken,
        turn: u64,
        step: u64,
        claimed: Vec<Message>,
    ) -> Result<PreStepDecision, EventError> {
        let bus = self
            .loop_ctx
            .event_bus()
            .ok_or_else(|| EventError::Listener("agent context has no event bus".into()))?;
        let event = Arc::new(AgentPreStep {
            turn,
            step,
            messages: claimed.clone(),
            signal: abort.clone(),
        });
        let inner_claimed = claimed.clone();
        bus.waterfall_scoped(&self.agent_ctx, event, move || {
            let inner_claimed = inner_claimed.clone();
            Box::pin(async move {
                Ok(PreStepDecision::Enter {
                    messages: inner_claimed,
                })
            })
        })
        .await
    }

    async fn turn_stopping(&self, turn: u64, abort: &CancellationToken) {
        let Some(bus) = self.loop_ctx.event_bus() else {
            return;
        };
        let event = Arc::new(AgentTurnStopping {
            turn,
            signal: abort.clone(),
        });
        let _ = bus.serial_scoped(&self.agent_ctx, event).await;
    }

    /// One model step: assemble, stream, and finalize an assistant message,
    /// then schedule any tool calls. `system` was fixed by this step's pre-step
    /// assembly (`SystemPromptRegistry::assemble`) and stays constant across
    /// request retries within the step.
    async fn step(
        &self,
        abort: &CancellationToken,
        turn: u64,
        step: u64,
        system: Option<&str>,
    ) -> Option<TurnEndReason> {
        // WI-9: one automatic condense-and-retry after a confirmed
        // context-window overflow (agent.max_overflow_retries, default 1).
        let mut overflow_recoveries = 0u32;
        let max_overflow_retries = self.max_overflow_retries();
        // Transient-failure retries for this step (see retry_transient_failure).
        let mut transient_retries = 0u32;
        'request: loop {
            let tools = self.tools.schemas_for(Some(&self.id.0));
            let assembled = match self.build_request(turn, step, abort, system, tools).await {
                Ok(assembled) => assembled,
                Err(error) => {
                    return Some(TurnEndReason::Error {
                        failure: error.to_failure(),
                    })
                }
            };
            let mut assembler = BlockAssembler::new();
            let mut chunk_seqs = Vec::new();
            let stream = match &assembled.prepared {
                Some(prepared) => prepared.stream(assembled.request.clone()),
                None => self.llm.stream(assembled.request.clone()),
            };
            let mut stream = Box::pin(stream);
            // WI-108: the chunk wait races the abort token — a hung provider
            // (no chunks in flight) still cancels immediately instead of at
            // the next chunk/error. `StreamExt::next` is cancel-safe, so the
            // select loses no chunks; `None` (stream end) exits the loop.
            loop {
            let chunk = tokio::select! {
                biased;
                _ = abort.cancelled() => {
                    self.append_interrupted_anchor(turn, step, &assembler, &chunk_seqs);
                    return Some(TurnEndReason::Aborted {
                        cause: self.take_cancel_cause(),
                    });
                }
                next = stream.next() => match next {
                    Some(chunk) => chunk,
                    None => break,
                },
            };
            let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        if abort.is_cancelled() {
                            self.append_interrupted_anchor(turn, step, &assembler, &chunk_seqs);
                            return Some(TurnEndReason::Aborted {
                                cause: self.take_cancel_cause(),
                            });
                        }
                        let failure = error.to_failure();
                        if failure.code == "CONTEXT_WINDOW_EXCEEDED"
                            && overflow_recoveries < max_overflow_retries
                        {
                            overflow_recoveries += 1;
                            self.compact_for_overflow(
                                system,
                                self.options.context_window.unwrap_or(0),
                            )
                            .await;
                            continue 'request;
                        }
                        match self
                            .retry_transient_failure(
                                turn,
                                step,
                                &assembled.request.provider,
                                &failure,
                                &self.step_retry_policy(&assembled),
                                transient_retries + 1,
                                abort,
                            )
                            .await
                        {
                            RetryDecision::Retry => {
                                transient_retries += 1;
                                continue 'request;
                            }
                            RetryDecision::Aborted => {
                                self.append_interrupted_anchor(
                                    turn, step, &assembler, &chunk_seqs,
                                );
                                return Some(TurnEndReason::Aborted {
                                    cause: self.take_cancel_cause(),
                                });
                            }
                            RetryDecision::GiveUp => {}
                        }
                        let action = self
                            .request_error_waterfall(
                                turn,
                                step,
                                &assembled.request,
                                &failure,
                                assembled
                                    .prepared
                                    .as_ref()
                                    .map(|prepared| prepared.retry_policy.clone()),
                                abort,
                            )
                            .await;
                        match action {
                            RequestErrorAction::Retry => continue 'request,
                            RequestErrorAction::None => {
                                return Some(TurnEndReason::Error { failure });
                            }
                        }
                    }
                };
                if abort.is_cancelled() {
                    self.append_interrupted_anchor(turn, step, &assembler, &chunk_seqs);
                    return Some(TurnEndReason::Aborted {
                        cause: self.take_cancel_cause(),
                    });
                }
                let seq = self
                    .session
                    .append_with(
                        "assistant/chunk",
                        JsonValue::Object(vec![
                            ("turn".into(), JsonValue::UInt(turn)),
                            ("step".into(), JsonValue::UInt(step)),
                            ("chunk".into(), chunk.to_json().expect("chunk json")),
                        ]),
                        AppendOptions::default(),
                    )
                    .expect("assistant/chunk append failed")
                    .seq;
                chunk_seqs.push(seq);
                assembler.push(chunk);
            }
            if abort.is_cancelled() {
                self.append_interrupted_anchor(turn, step, &assembler, &chunk_seqs);
                return Some(TurnEndReason::Aborted {
                    cause: self.take_cancel_cause(),
                });
            }
            let finish = assembler.finish();
            if matches!(
                finish,
                FinishReason::Error { .. } | FinishReason::Aborted { .. }
            ) {
                let failure = match &finish {
                    FinishReason::Error { failure } | FinishReason::Aborted { failure } => {
                        failure.clone()
                    }
                    _ => unreachable!("matched above"),
                };
                if failure.code == "CONTEXT_WINDOW_EXCEEDED"
                    && overflow_recoveries < max_overflow_retries
                {
                    overflow_recoveries += 1;
                    self.compact_for_overflow(system, self.options.context_window.unwrap_or(0))
                        .await;
                    continue 'request;
                }
                match self
                    .retry_transient_failure(
                        turn,
                        step,
                        &assembled.request.provider,
                        &failure,
                        &self.step_retry_policy(&assembled),
                        transient_retries + 1,
                        abort,
                    )
                    .await
                {
                    RetryDecision::Retry => {
                        transient_retries += 1;
                        continue 'request;
                    }
                    RetryDecision::Aborted => {
                        self.append_interrupted_anchor(turn, step, &assembler, &chunk_seqs);
                        return Some(TurnEndReason::Aborted {
                            cause: self.take_cancel_cause(),
                        });
                    }
                    RetryDecision::GiveUp => {}
                }
                let action = self
                    .request_error_waterfall(
                        turn,
                        step,
                        &assembled.request,
                        &failure,
                        assembled
                            .prepared
                            .as_ref()
                            .map(|prepared| prepared.retry_policy.clone()),
                        abort,
                    )
                    .await;
                match action {
                    RequestErrorAction::Retry => continue,
                    RequestErrorAction::None => {
                        return Some(TurnEndReason::Error { failure });
                    }
                }
            }
            let message = assembler.message(Some(MessageSource::Model {
                provider: assembled.request.provider.clone(),
                model: assembled.request.model.clone(),
                replay_state: assembler.replay_state().map(|state| {
                    JsonValue::Object(vec![
                        ("response".into(), state.response),
                        (
                            "blocks".into(),
                            state
                                .blocks
                                .map(JsonValue::Array)
                                .unwrap_or(JsonValue::Null),
                        ),
                    ])
                }),
            }));
            self.session
                .append_with(
                    "assistant/message",
                    JsonValue::Object(vec![
                        ("turn".into(), JsonValue::UInt(turn)),
                        ("step".into(), JsonValue::UInt(step)),
                        ("message".into(), message.to_json().expect("message json")),
                        (
                            "usage".into(),
                            assembler
                                .usage()
                                .map(usage_to_json)
                                .unwrap_or(JsonValue::Null),
                        ),
                    ]),
                    AppendOptions {
                        surface_op: Some(SurfaceOp::Append),
                        source_event_seqs: chunk_seqs,
                    },
                )
                .expect("assistant/message append failed");
            if matches!(finish, FinishReason::MaxTokens) {
                return Some(TurnEndReason::MaxTokens);
            }
            let tool_calls = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { .. } => Some(block.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            // WI-1: a reload_image call pulls its file id hot for the rest of
            // the turn — the next step's projection re-includes the image
            // (confirmed results only, see `confirm_reload_results`).
            // BUG-114/115: a file id already ATTEMPTED this turn marks its
            // call a duplicate — the pair is answered inline below with a
            // stop hint whose text depends on whether the earlier attempt
            // actually succeeded. BUG-135: every reload call (duplicate or
            // not) also burns the turn's storm budget.
            let mut duplicate_reload_calls: std::collections::HashSet<
                starriver_kernel::llm::CallId,
            > = std::collections::HashSet::new();
            let mut seen_reload_call_ids: std::collections::HashSet<
                starriver_kernel::llm::CallId,
            > = std::collections::HashSet::new();
            for call in &tool_calls {
                if let ContentBlock::ToolCall {
                    id,
                    name: Some(name),
                    arguments,
                } = call
                {
                    if name == crate::image_projection::RELOAD_IMAGE_TOOL {
                        // BUG-128: a repeated CallId (malformed model output)
                        // is a duplicate regardless of its arguments — the
                        // first call settles the id; a second same-id result
                        // would be ambiguous for id-pairing providers.
                        if !seen_reload_call_ids.insert(id.clone()) {
                            duplicate_reload_calls.insert(id.clone());
                            continue;
                        }
                        if let Some(file_id) =
                            crate::image_projection::parse_reload_file_id(arguments)
                        {
                            if !self.attempted_reload_ids.lock().unwrap().insert(file_id) {
                                duplicate_reload_calls.insert(id.clone());
                            }
                        }
                    }
                }
            }
            if tool_calls.is_empty() {
                return Some(TurnEndReason::Completed);
            }
            // The call list is walked in model order and segmented around the
            // duplicate reloads, so tool/call events stay faithful to the
            // assistant message while the repeats get the stop hint. The
            // walk keeps the iterator so the BUG-135 breaker can pair every
            // remaining call when it force-ends the turn.
            let mut concluded = false;
            let mut segment: Vec<ContentBlock> = Vec::new();
            let mut calls_iter = tool_calls.into_iter();
            while let Some(call) = calls_iter.next() {
                let is_reload = matches!(
                    &call,
                    ContentBlock::ToolCall { name: Some(name), .. }
                        if *name == crate::image_projection::RELOAD_IMAGE_TOOL
                );
                let duplicate = match &call {
                    ContentBlock::ToolCall { id, .. } if is_reload => {
                        duplicate_reload_calls.contains(id)
                    }
                    _ => false,
                };
                // BUG-135: count each reload call as the walk reaches it
                // (incremental, so the first BUDGET calls still execute).
                // The total budget covers fresh ids too — a model fabricating
                // a new id per step (endless NOT_FOUND) burns the same turn
                // budget as same-id repeats.
                let call_total = if is_reload {
                    self.reload_storm.lock().unwrap().note_call()
                } else {
                    0
                };
                let over_budget = is_reload
                    && call_total > crate::tool_calls::RELOAD_TURN_CALL_BUDGET;
                // BUG-138: non-reload calls feed the general repeat guard —
                // consecutive identical-failure breaker + the turn's
                // non-reload call budget as the varied-args backstop. Results
                // of the freshly flushed segment are absorbed first so the
                // check sees them.
                let general_break = if is_reload {
                    None
                } else {
                    self.turn_repeat
                        .lock()
                        .unwrap()
                        .absorb_results(&self.session);
                    let (name, arguments) = match &call {
                        ContentBlock::ToolCall {
                            name: Some(name),
                            arguments,
                            ..
                        } => (name.as_str(), arguments),
                        _ => return Some(TurnEndReason::Completed),
                    };
                    let sig = crate::tool_calls::call_signature(name, arguments);
                    let mut guard = self.turn_repeat.lock().unwrap();
                    let fails = guard.consecutive_errors(&sig);
                    if fails >= crate::tool_calls::REPEAT_FAILURE_LIMIT {
                        Some(format!(
                            "tool `{name}` failed identically {fails} times in a row"
                        ))
                    } else if guard.over_budget(
                        // Settings seam: `agent.turn_tool_call_budget`
                        // overrides the engine default (666) per agent.
                        self.options
                            .turn_tool_call_budget
                            .unwrap_or(crate::tool_calls::TURN_TOOL_CALL_BUDGET),
                    ) {
                        Some("turn tool-call budget exhausted".into())
                    } else {
                        let _ = guard.note_call();
                        None
                    }
                };
                if !duplicate && !over_budget && general_break.is_none() {
                    segment.push(call);
                    continue;
                }
                if !segment.is_empty() {
                    match self
                        .run_tool_segment(turn, step, abort, std::mem::take(&mut segment))
                        .await
                    {
                        Ok(segment_concluded) => concluded |= segment_concluded,
                        Err(error) => {
                            return Some(TurnEndReason::Error {
                                failure: tool_failure(&error),
                            })
                        }
                    }
                }
                // Absorb the flushed segment's reload results before wording
                // the stop hint: confirmed ids say "already re-attached",
                // failed ones say "attempted without success".
                self.confirm_reload_results();
                if let Some(reason) = general_break {
                    // BUG-138: the generalized breaker — this call gets the
                    // terminal error pair, every remaining call is paired as
                    // skipped, and the turn ends (a model that keeps re-emitting
                    // a failing identical call cannot hold the turn hostage).
                    crate::tool_calls::append_repeat_storm_break(
                        &self.session,
                        turn,
                        step,
                        &call,
                        &reason,
                    );
                    for remaining in calls_iter {
                        crate::tool_calls::append_call_skipped_by_breaker(
                            &self.session,
                            turn,
                            step,
                            &remaining,
                        );
                    }
                    self.confirm_reload_results();
                    return Some(TurnEndReason::Completed);
                }
                let file_id = match &call {
                    ContentBlock::ToolCall { arguments, .. } => {
                        crate::image_projection::parse_reload_file_id(arguments)
                            .unwrap_or_default()
                    }
                    _ => String::new(),
                };
                // BUG-135 breaker: same-id noops beyond the limit, or the
                // turn's total reload calls over budget → terminal error
                // pair, pair the remaining calls, END THE TURN. A model that
                // ignores the noop stop hint cannot keep the turn alive.
                let breaker = over_budget
                    || self
                        .reload_storm
                        .lock()
                        .unwrap()
                        .note_noop(&file_id)
                        > crate::tool_calls::RELOAD_NOOP_LIMIT_PER_ID;
                if breaker {
                    crate::tool_calls::append_reload_storm_break(
                        &self.session,
                        turn,
                        step,
                        &call,
                        &file_id,
                    );
                    for remaining in calls_iter {
                        crate::tool_calls::append_call_skipped_by_breaker(
                            &self.session,
                            turn,
                            step,
                            &remaining,
                        );
                    }
                    self.confirm_reload_results();
                    return Some(TurnEndReason::Completed);
                }
                // BUG-131: once the turn is aborted the remaining noop pairs
                // land in the ABORTED shape (matching the generic scheduler's
                // before-dispatch marker) instead of a success-looking hint.
                let aborted = abort.is_cancelled();
                let confirmed = self.reload_ids.lock().unwrap().contains(&file_id);
                crate::tool_calls::append_duplicate_reload_noop(
                    &self.session,
                    turn,
                    step,
                    &call,
                    &file_id,
                    confirmed,
                    aborted,
                );
            }
            if !segment.is_empty() {
                match self.run_tool_segment(turn, step, abort, segment).await {
                    Ok(segment_concluded) => concluded |= segment_concluded,
                    Err(error) => {
                        return Some(TurnEndReason::Error {
                            failure: tool_failure(&error),
                        })
                    }
                }
            }
            self.confirm_reload_results();
            return if concluded {
                Some(TurnEndReason::Completed)
            } else {
                None
            };
        }
    }

    /// WI-160 (BUG-115): pin a reload_image file id into `reload_ids` only
    /// once its `tool/result` settled WITHOUT an error. Scans the session
    /// log forward from a monotonic cursor, joining `tool/call` rows
    /// (call seq → file id, synthetic duplicate-noop rows excluded) with
    /// their linked results. A NOT_FOUND attempt therefore never pins, and
    /// the projection keeps the honest "call reload_image" placeholder
    /// instead of a displaced no-retry variant.
    fn confirm_reload_results(&self) {
        use std::collections::HashMap;
        let cursor = *self.reload_confirm_cursor.lock().unwrap();
        let mut calls: HashMap<u64, String> = HashMap::new();
        let mut results: HashMap<u64, bool> = HashMap::new();
        let mut last_seq = cursor;
        // Tail scan only: `events_after` skips the already-confirmed prefix
        // instead of deep-cloning the whole log each step.
        for event in self.session.events_after(cursor) {
            last_seq = last_seq.max(event.seq);
            match event.event_type.as_str() {
                "tool/call" => {
                    let is_reload = event.data.as_str_at("name")
                        == Some(crate::image_projection::RELOAD_IMAGE_TOOL);
                    let is_noop =
                        event.data.get("duplicateNoop").and_then(|v| v.as_bool()) == Some(true);
                    let Some(file_id) = event
                        .data
                        .get("arguments")
                        .and_then(|args| crate::image_projection::parse_reload_file_id(args))
                    else {
                        continue;
                    };
                    if is_reload && !is_noop {
                        calls.insert(event.seq, file_id);
                    }
                }
                "tool/result" => {
                    if let Some(&linked) = event.source_event_seqs.first() {
                        let ok = event.data.get("error").is_none();
                        results.insert(linked, ok);
                    }
                }
                _ => {}
            }
        }
        if last_seq > cursor {
            *self.reload_confirm_cursor.lock().unwrap() = last_seq;
        }
        if calls.is_empty() {
            return;
        }
        let mut pins = self.reload_ids.lock().unwrap();
        for (seq, file_id) in calls {
            if results.get(&seq) == Some(&true) {
                pins.insert(file_id);
            }
        }
    }

    /// Execute one in-order segment of the step's tool calls through the
    /// scheduler. BUG-114 segmentation: duplicate reload_image calls are
    /// answered inline by the caller; everything else flows through here so
    /// the generic scheduler stays reload-agnostic.
    async fn run_tool_segment(
        &self,
        turn: u64,
        step: u64,
        abort: &CancellationToken,
        segment: Vec<ContentBlock>,
    ) -> Result<bool, ToolError> {
        crate::tool_calls::execute_tool_calls(
            crate::tool_calls::ToolCallStep {
                tools: self.tools.as_ref(),
                session: &self.session,
                agent_id: &self.id.0,
                turn,
                step,
            },
            segment,
            abort,
            // Backlog #7 settings seam: per-agent budget from options
            // (preset reads `agent.max_parallel_tool_calls`), loop
            // default otherwise.
            // A budget of 0 makes the start-loop guard unsatisfiable
            // (`in_flight.len() < 0`) with no await in the cycle — clamp
            // to serial instead of busy-spinning the driver forever.
            self.options
                .max_parallel_tool_calls
                .unwrap_or(crate::tool_calls::DEFAULT_MAX_PARALLEL_TOOL_CALLS)
                .max(1),
            &|context| {
                let mut inbox = self.inbox_lock();
                let len = inbox.next_step().len();
                inbox.splice(InboxTarget::NextStep, len, 0, vec![context]);
            },
        )
        .await
    }

    async fn build_request(
        &self,
        turn: u64,
        step: u64,
        abort: &CancellationToken,
        system: Option<&str>,
        tools: Vec<ToolSchema>,
    ) -> Result<AssembledRequest, LlmError> {
        let bus = self.loop_ctx.event_bus().ok_or_else(|| LlmError::Call {
            code: "EVENT".into(),
            message: "agent context has no event bus".into(),
            status: None,
            provider_retry_after_ms: None,
            request_id: None,
        })?;
        // Proposal chain: the next step's seed is the previous effective
        // config minus the fields the adapter materialized last time, so
 // explicit edits survive multi-step turns (requestProposal).
        let seed = match self.last_proposal.lock().unwrap().clone() {
            Some(mut previous) => {
                let defaults = *self.last_adapter_defaults.lock().unwrap();
                if defaults.reasoning_effort {
                    previous.reasoning_effort = None;
                }
                if defaults.max_tokens {
                    previous.max_tokens = None;
                }
                previous
            }
            None => LlmCallConfig {
                provider: self.options.provider.clone().unwrap_or_default(),
                model: self.options.model.clone().unwrap_or_default(),
                reasoning_effort: None,
                temperature: None,
                max_tokens: self.options.max_tokens,
                stop: None,
            },
        };
        if seed.provider.is_empty() || seed.model.is_empty() {
            return Err(LlmError::Call {
                code: "NO_ROUTE".into(),
                message: "agent has no provider/model".into(),
                status: None,
                provider_retry_after_ms: None,
                request_id: None,
            });
        }
        let proposed = {
            let inner = seed.clone();
            bus.waterfall_scoped(
                &self.agent_ctx,
                Arc::new(AgentRequest {
                    turn,
                    step,
                    signal: abort.clone(),
                }),
                move || {
                    let inner = inner.clone();
                    Box::pin(async move { Ok(inner) })
                },
            )
            .await
            .map_err(|error| LlmError::Call {
                code: "EVENT".into(),
                message: error.to_string(),
                status: None,
                provider_retry_after_ms: None,
                request_id: None,
            })?
        };
        let prepared = match self
            .llm
            .prepare_call(proposed.clone(), Some(abort.clone()))
            .await
        {
            Ok(prepared) => Some(prepared),
            Err(error) if error.code() == "NO_ADAPTER" => None,
            Err(error) => return Err(error),
        };
        let config = prepared
            .as_ref()
            .map(|prepared| prepared.config.clone())
            .unwrap_or_else(|| proposed.clone());
        *self.last_adapter_defaults.lock().unwrap() = prepared
            .as_ref()
            .map(|prepared| prepared.adapter_defaults)
            .unwrap_or_default();
        *self.last_proposal.lock().unwrap() = Some(config.clone());

        let key = HeaderKey {
            config: config.clone(),
            system: system.map(String::from),
            tool_names: tools.iter().map(|tool| tool.name.clone()).collect(),
        };
        let header_changed = self.last_header.lock().unwrap().as_ref() != Some(&key);
        if !self.header_logged.load(Ordering::SeqCst) || header_changed {
            let reason = if self.header_logged.load(Ordering::SeqCst) {
                "change"
            } else {
                "initial"
            };
            let mut header = vec![
                ("config".into(), config_to_json(&config)),
                ("reason".into(), JsonValue::String(reason.into())),
            ];
            if let Some(system) = system {
                header.push(("system".into(), JsonValue::String(system.into())));
            }
            if !tools.is_empty() {
                header.push(("tools".into(), schemas_to_json(&tools)));
            }
            if let Some(prepared) = &prepared {
                if prepared.adapter_defaults.reasoning_effort
                    || prepared.adapter_defaults.max_tokens
                {
                    header.push((
                        "adapterDefaults".into(),
                        JsonValue::Object(vec![
                            (
                                "reasoningEffort".into(),
                                JsonValue::Bool(prepared.adapter_defaults.reasoning_effort),
                            ),
                            (
                                "maxTokens".into(),
                                JsonValue::Bool(prepared.adapter_defaults.max_tokens),
                            ),
                        ]),
                    ));
                }
            }
            self.session
                .append_with(
                    "request/header",
                    JsonValue::Object(header),
                    AppendOptions::default(),
                )
                .expect("request/header append failed");
            self.header_logged.store(true, Ordering::SeqCst);
            *self.last_header.lock().unwrap() = Some(key);
        }

        let mut messages = self
            .session
            .derive_messages()
            .map_err(|error| LlmError::Call {
                code: "EVENT".into(),
                message: error.to_string(),
                status: None,
                provider_retry_after_ms: None,
                request_id: None,
            })?;
        // WI-1/WI-159: cold/hot image projection — budgeted images resolve
        // to data URLs; the rest ship as placeholder text. Per-step
        // re-projection is what makes a reload take effect on the very next
        // request. BUG-118: a configured projection with a MISSING resolver
        // must not silently skip the engine (fileId-only Image blocks would
        // ride the wire raw) — degrade through the MissingResolver stub so
        // every fileId slot turns into its cold placeholder instead.
        if let Some(cfg) = self.options.image_projection {
            const MISSING: crate::image_projection::MissingResolver =
                crate::image_projection::MissingResolver;
            match self.options.image_resolver.as_ref() {
                Some(resolver) => crate::image_projection::project_images(
                    &mut messages,
                    &cfg,
                    resolver.0.as_ref(),
                    &self.reload_ids.lock().unwrap(),
                ),
                None => crate::image_projection::project_images(
                    &mut messages,
                    &cfg,
                    &MISSING,
                    &self.reload_ids.lock().unwrap(),
                ),
            }
        }
        let request = GenerateOptions {
            provider: config.provider.clone(),
            model: config.model.clone(),
            reasoning_effort: config.reasoning_effort.clone(),
            messages,
            system: system.map(String::from),
            tools: if tools.is_empty() { None } else { Some(tools) },
            temperature: config.temperature,
            max_tokens: config.max_tokens,
            stop: config.stop.clone(),
            session_id: Some(self.id.0.clone()),
            agent_id: None,
            tool_choice: None,
            cancellation: Some(abort.clone()),
            purpose: None,
            loop_assembled: true,
        };
        Ok(AssembledRequest { request, prepared })
    }

    /// 2026-10-04: transient-failure retry for one model step. The kernel's
    /// RetryPolicy (provider-configurable, defaults SERVER/TIMEOUT/TRANSPORT/
    /// RATE_LIMIT/EMPTY_RESPONSE/PROVIDER_STREAM_TRUNCATED ×5, 500ms→10s
    /// jittered backoff) was plumbed all the way into every request — and
    /// then never executed: the AgentRequestError waterfall has no production
    /// listener, so its default action ended the turn on the FIRST blip. A
    /// provider 500 "network error, please retry later" killed a 5m45s turn mid-fix
    /// (session 72a13c58 turn 26, host.log 2026-10-04 05:29:31Z) with no
    /// error surface in the UI. The policy now executes here — the per-step
    /// attempt counter and the abort-aware backoff sleep live naturally in
    /// the request loop — and the waterfall still runs afterwards, keeping
    /// hosts and plugins the final say (their Retry still wins).
    async fn retry_transient_failure(
        &self,
        turn: u64,
        step: u64,
        provider: &str,
        failure: &LlmFailure,
        policy: &RetryPolicy,
        attempt: u32,
        abort: &CancellationToken,
    ) -> RetryDecision {
        let max_retries = match policy {
            RetryPolicy::Normal { max_retries, .. } => *max_retries,
            RetryPolicy::Always { .. } => u32::MAX,
        };
        if attempt > max_retries || !policy.is_retryable(&failure.code) {
            return RetryDecision::GiveUp;
        }
        let mut delay = backoff_delay(policy.backoff(), attempt);
        if let Some(retry_after) = failure.provider_retry_after_ms {
            delay = delay.max(std::time::Duration::from_millis(retry_after));
        }
        tracing::warn!(
            turn,
            step,
            provider,
            code = %failure.code,
            attempt,
            max_retries,
            delay_ms = delay.as_millis() as u64,
            "transient model-call failure; retrying the request"
        );
        tokio::select! {
            biased;
            _ = abort.cancelled() => RetryDecision::Aborted,
            _ = tokio::time::sleep(delay) => RetryDecision::Retry,
        }
    }

    /// The retry policy for this step's request: the provider's prepared
    /// policy when one was resolved, otherwise the kernel defaults (mirrors
    /// `resolve_retry_policy`'s omission-means-default semantics).
    fn step_retry_policy(&self, assembled: &AssembledRequest) -> RetryPolicy {
        assembled
            .prepared
            .as_ref()
            .map(|prepared| prepared.retry_policy.clone())
            .unwrap_or_default()
    }

    async fn request_error_waterfall(
        &self,
        turn: u64,
        step: u64,
        request: &GenerateOptions,
        failure: &LlmFailure,
        retry_policy: Option<RetryPolicy>,
        abort: &CancellationToken,
    ) -> RequestErrorAction {
        let Some(bus) = self.loop_ctx.event_bus() else {
            return RequestErrorAction::None;
        };
        let event = Arc::new(AgentRequestError {
            turn,
            step,
            provider: request.provider.clone(),
            failure: failure.clone(),
            retry_policy,
            signal: abort.clone(),
        });
        bus.waterfall_scoped(&self.agent_ctx, event, || {
            Box::pin(async { Ok(RequestErrorAction::None) })
        })
        .await
        .unwrap_or(RequestErrorAction::None)
    }

    fn append_interrupted_anchor(
        &self,
        turn: u64,
        step: u64,
        assembler: &BlockAssembler,
        chunk_seqs: &[u64],
    ) {
        let content = assembler.interrupted_blocks();
        if content.is_empty() {
            return;
        }
        let message = Message {
            id: mint_message_id(),
            role: MessageRole::Assistant,
            content,
            source: MessageSource::Model {
                provider: self.options.provider.clone().unwrap_or_default(),
                model: self.options.model.clone().unwrap_or_default(),
                replay_state: None,
            },
        };
        self.session
            .append_with(
                "assistant/message",
                JsonValue::Object(vec![
                    ("turn".into(), JsonValue::UInt(turn)),
                    ("step".into(), JsonValue::UInt(step)),
                    ("message".into(), message.to_json().expect("message json")),
                    ("interrupted".into(), JsonValue::Bool(true)),
                ]),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: chunk_seqs.to_vec(),
                },
            )
            .expect("interrupted assistant/message append failed");
    }

    fn emit_agent_error(&self, turn: u64, step: u64, failure: &LlmFailure) {
        let Some(bus) = self.loop_ctx.event_bus() else {
            return;
        };
        bus.emit_scoped(
            &self.agent_ctx,
            Arc::new(AgentErrorEvent {
                turn,
                step,
                error: failure.message.clone(),
            }),
        );
    }

    fn wake_driver(&self) {
        self.state.wake.add_permits(1);
    }

    /// Lock the inbox, recovering from a poisoned mutex. Inbox validation
    /// panics happen before any projection mutation, so the state is still
    /// consistent and `into_inner` is safe.
    fn inbox_lock(&self) -> std::sync::MutexGuard<'_, Inbox> {
        self.inbox
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn take_cancel_cause(&self) -> AgentCancelCause {
        self.last_cancel.lock().unwrap().take().unwrap_or_else(|| {
            AgentCancelCause::new(starriver_kernel::agent::AgentCancelKind::User)
        })
    }

    fn emit_agent_status(&self, status: AgentStatus) {
        let Some(bus) = self.loop_ctx.event_bus() else {
            return;
        };
        let agent: Arc<dyn Agent> = self.me.lock().unwrap().upgrade().expect("agent alive");
        bus.emit_scoped(
            &self.agent_ctx,
            Arc::new(AgentStatusChanged { agent, status }),
        );
    }
}

fn initial_last_turn(session: &Session) -> u64 {
    session
        .events()
        .iter()
        .rev()
        .find(|event| event.event_type == "turn/start")
        .and_then(|event| {
            event
                .data
                .get("turn")
                .and_then(starriver_kernel::json::JsonValue::as_u64)
        })
        .unwrap_or(0)
}

/// Outcome of `retry_transient_failure` for one failed model call.
enum RetryDecision {
    /// Policy covers this failure and attempts remain — the step re-runs the
    /// request after the backoff sleep.
    Retry,
    /// The abort token fired during the backoff sleep.
    Aborted,
    /// Not retryable under the policy, or the attempt budget is spent.
    GiveUp,
}

/// Local delay for retry `attempt` (1-based): `initial << (attempt-1)`
/// bounded by `max`, with symmetric ±`jitter_ratio` jitter. Jitter comes from
/// the wall clock (no rand dependency) — any spread breaks thundering herds.
fn backoff_delay(backoff: &RetryBackoff, attempt: u32) -> std::time::Duration {
    let shift = (attempt.saturating_sub(1)).min(16);
    let base = backoff
        .initial_delay_ms
        .saturating_mul(1u64 << shift)
        .min(backoff.max_delay_ms);
    let factor = if backoff.jitter_ratio <= 0.0 {
        1.0
    } else {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        1.0 + backoff.jitter_ratio * (2.0 * (nanos % 1000) as f64 / 1000.0 - 1.0)
    };
    std::time::Duration::from_millis(((base as f64) * factor).max(0.0) as u64)
}

fn event_failure(error: &EventError) -> LlmFailure {
    LlmFailure {
        message: error.to_string(),
        code: "EVENT".into(),
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    }
}

fn tool_failure(error: &ToolError) -> LlmFailure {
    LlmFailure {
        message: error.to_string(),
        code: error.code().to_string(),
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    }
}

fn usage_to_json(usage: TokenUsage) -> JsonValue {
    JsonValue::Object(vec![
        ("inputTokens".into(), JsonValue::UInt(usage.input_tokens)),
        ("outputTokens".into(), JsonValue::UInt(usage.output_tokens)),
        (
            "cacheReadTokens".into(),
            usage
                .cache_read_tokens
                .map(JsonValue::UInt)
                .unwrap_or(JsonValue::Null),
        ),
        (
            "cacheWriteTokens".into(),
            usage
                .cache_write_tokens
                .map(JsonValue::UInt)
                .unwrap_or(JsonValue::Null),
        ),
        (
            "reasoningTokens".into(),
            usage
                .reasoning_tokens
                .map(JsonValue::UInt)
                .unwrap_or(JsonValue::Null),
        ),
    ])
}

fn config_to_json(config: &LlmCallConfig) -> JsonValue {
    let mut entries = vec![
        (
            "provider".into(),
            JsonValue::String(config.provider.clone()),
        ),
        ("model".into(), JsonValue::String(config.model.clone())),
    ];
    if let Some(effort) = &config.reasoning_effort {
        entries.push(("reasoningEffort".into(), JsonValue::String(effort.clone())));
    }
    if let Some(temperature) = config.temperature {
        entries.push(("temperature".into(), JsonValue::Float(temperature)));
    }
    if let Some(max_tokens) = config.max_tokens {
        entries.push(("maxTokens".into(), JsonValue::UInt(max_tokens)));
    }
    if let Some(stop) = &config.stop {
        entries.push((
            "stop".into(),
            JsonValue::Array(stop.iter().cloned().map(JsonValue::String).collect()),
        ));
    }
    JsonValue::Object(entries)
}

fn schemas_to_json(schemas: &[ToolSchema]) -> JsonValue {
    JsonValue::Array(
        schemas
            .iter()
            .map(|schema| {
                JsonValue::Object(vec![
                    ("name".into(), JsonValue::String(schema.name.clone())),
                    (
                        "description".into(),
                        JsonValue::String(schema.description.clone()),
                    ),
                    ("parameters".into(), schema.parameters.clone()),
                ])
            })
            .collect(),
    )
}

impl Agent for ReactLoopAgent {
    fn id(&self) -> &SessionId {
        &self.id
    }
    fn options(&self) -> &AgentOptions {
        &self.options
    }
    fn session(&self) -> &Session {
        &self.session
    }
    fn status(&self) -> AgentStatus {
        match &*self.phase.lock().unwrap() {
            Phase::Idle { .. } => AgentStatus::Idle,
            Phase::Running { .. } => AgentStatus::Running,
        }
    }
    fn ctx(&self) -> &Context {
        &self.agent_ctx
    }
    fn followup(&self, message: Message) {
        self.send(message, InboxTarget::NextTurn, true);
    }
    fn steer(&self, message: Message) {
        self.send(message, InboxTarget::NextStep, true);
    }
    fn inject(&self, message: Message) {
        self.send(message, InboxTarget::NextStep, false);
    }
    fn send(&self, message: Message, target: InboxTarget, wakeup: bool) {
        let mut inbox = self.inbox_lock();
        let len = match target {
            InboxTarget::NextTurn => inbox.next_turn().len(),
            InboxTarget::NextStep => inbox.next_step().len(),
        };
        inbox.splice(target, len, 0, vec![message]);
        drop(inbox);
        if wakeup {
            self.state.parked.store(false, Ordering::SeqCst);
            self.wake_driver();
        }
    }
    fn cancel(&self, cause: AgentCancelCause, keep_inbox: bool) -> bool {
        if !keep_inbox {
            self.inbox_lock().clear();
        } else {
            self.state.parked.store(true, Ordering::SeqCst);
        }
        let mut phase = self.phase.lock().unwrap();
        if let Phase::Running { abort, .. } = &mut *phase {
            // Record the cause only when a live turn is actually aborted, so an
            // idle no-op cancel cannot leak a stale cause into a later turn.
            *self.last_cancel.lock().unwrap() = Some(cause);
            abort.cancel();
            true
        } else {
            false
        }
    }
    fn force_compact(&self, system: Option<&str>, window: u64) -> BoxFuture<Option<(u64, u64)>> {
        let me = self
            .me
            .lock()
            .unwrap()
            .upgrade()
            .expect("agent must be alive");
        let system = system.map(str::to_string);
        Box::pin(async move { me.force_compact(system.as_deref(), window).await })
    }

    fn when_idle(&self) -> BoxFuture<()> {
        let me = self
            .me
            .lock()
            .unwrap()
            .upgrade()
            .expect("agent must be alive");
        Box::pin(async move {
            loop {
                if !me.state.running.load(Ordering::SeqCst) {
                    // Re-check after a yield so a run scheduled just before
 // this read is included, matching whenIdle contract.
                    tokio::task::yield_now().await;
                    if !me.state.running.load(Ordering::SeqCst) {
                        return;
                    }
                    continue;
                }
                me.state.run_done.notified().await;
            }
        })
    }

    fn pending_messages(&self) -> PendingMessages {
        let inbox = self.inbox_lock();
        PendingMessages {
            next_turn: inbox.next_turn().to_vec(),
            next_step: inbox.next_step().to_vec(),
        }
    }

    fn pending_remove(&self, id: &str) -> bool {
        self.inbox_lock().remove(&MessageId(id.to_string()))
    }

    fn pending_update_text(&self, id: &str, text: &str) -> bool {
        let updated = Message {
            id: MessageId(id.to_string()),
            role: MessageRole::User,
            content: vec![ContentBlock::Text(text.to_string())],
            source: MessageSource::User,
        };
        self.inbox_lock().replace(&MessageId(id.to_string()), updated)
    }

    fn pending_replace(&self, id: &str, content: Vec<ContentBlock>) -> bool {
        // The replace must keep the queue's user-message identity; only the
        // content changes (the host rebuilds it after image admission).
        let replaced = Message {
            id: MessageId(id.to_string()),
            role: MessageRole::User,
            content,
            source: MessageSource::User,
        };
        self.inbox_lock().replace(&MessageId(id.to_string()), replaced)
    }

    fn pending_flush(&self) {
        self.state.parked.store(false, Ordering::SeqCst);
        self.wake_driver();
    }

    fn is_parked(&self) -> bool {
        self.state.parked.load(Ordering::SeqCst)
    }

    fn force_compact_keep_recent(
        &self,
        system: Option<&str>,
        window: u64,
        keep_recent: u32,
    ) -> BoxFuture<Option<(u64, u64)>> {
        let me = self
            .me
            .lock()
            .unwrap()
            .upgrade()
            .expect("agent must be alive");
        let system = system.map(str::to_string);
        Box::pin(async move {
            me.force_compact_keep_recent_inner(system.as_deref(), window, keep_recent)
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_rslite::{Context, EventBus};
    use starriver_kernel::agent::{AgentCancelKind, AgentStatus};
    use starriver_kernel::llm::{
        CallId, ContentBlock, FinishReason, LlmRuntime, MessageId, MessageRole, MessageSource,
    };
    use starriver_kernel::session::{SessionId, SessionStore};
    use starriver_kernel::tools::ToolRuntime;

    fn message(text: &str) -> Message {
        Message {
            id: MessageId(text.into()),
            role: MessageRole::User,
            content: vec![ContentBlock::Text(text.into())],
            source: MessageSource::User,
        }
    }

    fn agent_arc() -> Arc<ReactLoopAgent> {
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let session = Session::new(SessionId("a1".into()), bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(EventBus::new()))
            as Arc<dyn LlmRuntimeView>;
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn ToolRuntimeView>;
        ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            AgentOptions::default(),
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        )
    }

    /// Adapter that records the outgoing request's `system` string.
    struct SystemCaptureAdapter {
        providers: Vec<String>,
        systems: Arc<Mutex<Vec<Option<String>>>>,
        stream_chunks: Vec<starriver_kernel::llm::StreamChunk>,
    }
    impl SystemCaptureAdapter {
        fn stop_after_text() -> Vec<starriver_kernel::llm::StreamChunk> {
            use starriver_kernel::llm::{ContentBlock, FinishReason, StreamChunk};
            vec![
                StreamChunk::TextDelta {
                    index: 0,
                    text: "ok".into(),
                },
                StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::Text("ok".into()),
                },
                StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                },
            ]
        }
    }
    impl starriver_kernel::llm::LlmAdapter for SystemCaptureAdapter {
        fn providers(&self) -> &[String] {
            &self.providers
        }
        fn stream(
            &self,
            options: starriver_kernel::llm::GenerateOptions,
        ) -> futures::stream::BoxStream<
            'static,
            Result<starriver_kernel::llm::StreamChunk, starriver_kernel::llm::LlmError>,
        > {
            self.systems.lock().unwrap().push(options.system.clone());
            Box::pin(futures::stream::iter(
                self.stream_chunks
                    .iter()
                    .cloned()
                    .map(Ok)
                    .collect::<Vec<_>>(),
            ))
        }
    }

    #[tokio::test]
    async fn registry_sections_and_contexts_flow_into_request() {
        use starriver_kernel::system_prompt::{SectionEntry, SectionText};
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let systems = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let _adapter = llm
            .register_adapter(Arc::new(SystemCaptureAdapter {
                providers: vec!["mock".into()],
                systems: Arc::clone(&systems),
                stream_chunks: SystemCaptureAdapter::stop_after_text(),
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let prompts = Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new());
        prompts
            .section(
                &ctx,
                SectionEntry {
                    name: "identity".into(),
                    order: -100,
                    complete: false,
                    text: SectionText::Static("You are an AI agent powered by Starriver.".into()),
                },
            )
            .unwrap();
        prompts
            .context(
                &ctx,
                starriver_kernel::context::PromptContext {
                    name: "cwd".into(),
                    order: 1,
                    render: Arc::new(|_| "/tmp/proj".into()),
                },
            )
            .unwrap();
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::clone(&prompts),
        );
        agent.start();
        agent.followup(message("hi"));
        agent.when_idle().await;

        let systems = systems.lock().unwrap();
        assert_eq!(
            systems.last().and_then(|s| s.as_deref()),
            Some("You are an AI agent powered by Starriver."),
            "the assembled system must reach every request unchanged"
        );
        // The registry context contributes to the pre-step history snapshot
        // (claimed input lands first now; find the snapshot by its header).
        let events = agent.session().events();
        let snapshot = events
            .iter()
            .find(|e| {
                e.event_type == "user/message"
                    && format!("{:?}", e.data).contains("Current runtime context")
            })
            .expect("snapshot message");
        let rendered = format!("{:?}", snapshot.data);
        assert!(
            rendered.contains("Current runtime context") && rendered.contains("/tmp/proj"),
            "runtime-context snapshot must carry registry contributions"
        );
    }

    #[tokio::test]
    async fn empty_registry_falls_back_to_legacy_system_option() {
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let systems = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let _adapter = llm
            .register_adapter(Arc::new(SystemCaptureAdapter {
                providers: vec!["mock".into()],
                systems: Arc::clone(&systems),
                stream_chunks: SystemCaptureAdapter::stop_after_text(),
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                system: Some("legacy base prompt".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        agent.start();
        agent.followup(message("hi"));
        agent.when_idle().await;
        assert_eq!(
            systems.lock().unwrap().last().and_then(|s| s.as_deref()),
            Some("legacy base prompt"),
            "unpopulated registry keeps serving the legacy whole-string system"
        );
    }

    #[tokio::test]
    async fn unknown_variable_in_section_ends_turn_with_error() {
        use starriver_kernel::system_prompt::{SectionEntry, SectionText};
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let systems = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let _adapter = llm
            .register_adapter(Arc::new(SystemCaptureAdapter {
                providers: vec!["mock".into()],
                systems: Arc::clone(&systems),
                stream_chunks: SystemCaptureAdapter::stop_after_text(),
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let prompts = Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new());
        prompts
            .section(
                &ctx,
                SectionEntry {
                    name: "broken".into(),
                    order: 0,
                    complete: false,
                    text: SectionText::Static("hello {{who}}".into()),
                },
            )
            .unwrap();
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::clone(&prompts),
        );
        agent.start();
        agent.followup(message("hi"));
        agent.when_idle().await;

        assert!(
            systems.lock().unwrap().is_empty(),
            "no model request may go out when assembly fails"
        );
        let events = agent.session().events();
        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("error"),
            "assembly failure must surface as a turn error"
        );
    }

    #[tokio::test]
    async fn builtin_variables_provider_model_cwd_interpolate_into_system() {
        use starriver_kernel::system_prompt::{SectionEntry, SectionText};
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let systems = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let _adapter = llm
            .register_adapter(Arc::new(SystemCaptureAdapter {
                providers: vec!["mock".into()],
                systems: Arc::clone(&systems),
                stream_chunks: SystemCaptureAdapter::stop_after_text(),
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        // A prepared session carries the frozen workspace root in its header.
        let store = starriver_session::InMemorySessionStore::new(
            bus.clone(),
            Context::new().with_event_bus(&EventBus::new()),
        );
        let session = store
            .prepare(
                SessionId("a1".into()),
                starriver_kernel::session::PrepareOptions {
                    cwd: Some("/w/proj".into()),
                    seed: Vec::new(),
            agent_preset: None,
        },
            )
            .unwrap()
            .session
            .clone();
        let prompts = Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new());
        prompts
            .section(
                &ctx,
                SectionEntry {
                    name: "template".into(),
                    order: 0,
                    complete: false,
                    text: SectionText::Static(
                        "{{provider}} @ {{model}} @ {{cwd}} {{NotAVar}}".into(),
                    ),
                },
            )
            .unwrap();
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::clone(&prompts),
        );
        agent.start();
        agent.followup(message("hi"));
        agent.when_idle().await;
        assert_eq!(
            systems.lock().unwrap().last().and_then(|s| s.as_deref()),
            Some("mock @ m1 @ /w/proj {{NotAVar}}"),
            "the loop must supply the three builtin variables (cwd from the \
             frozen session header); malformed references stay literal"
        );
    }

    #[tokio::test]
    async fn model_variable_tracks_switched_model_across_republish() {
        // WI-49: a /model switch takes effect via the next execute, which
        // re-publishes the agent (same session id → same scoped variable
        // layer) with fresh options. The {{model}}/{{provider}} providers
        // read the ASSEMBLING agent's options, so the next step's system
        // carries the new route pair even though the first publish's
        // registrations are still the live ones.
        use starriver_kernel::system_prompt::{SectionEntry, SectionText};
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let systems = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let _adapter = llm
            .register_adapter(Arc::new(SystemCaptureAdapter {
                providers: vec!["mock".into()],
                systems: Arc::clone(&systems),
                stream_chunks: SystemCaptureAdapter::stop_after_text(),
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let store = starriver_session::InMemorySessionStore::new(
            bus.clone(),
            Context::new().with_event_bus(&EventBus::new()),
        );
        let session = store
            .prepare(
                SessionId("a2".into()),
                starriver_kernel::session::PrepareOptions {
                    cwd: Some("/w/proj".into()),
                    seed: Vec::new(),
            agent_preset: None,
        },
            )
            .unwrap()
            .session
            .clone();
        let prompts = Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new());
        prompts
            .section(
                &ctx,
                SectionEntry {
                    name: "template".into(),
                    order: 0,
                    complete: false,
                    text: SectionText::Static("{{provider}} @ {{model}}".into()),
                },
            )
            .unwrap();
        // First publish: model m1.
        let llm_view: Arc<dyn crate::services::LlmRuntimeView> = llm.clone();
        let agent_one = ReactLoopAgent::new(
            ctx.clone(),
            SessionId("a2".into()),
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session.clone(),
            Arc::clone(&llm_view),
            Arc::clone(&tools),
            Arc::clone(&prompts),
        );
        agent_one.start();
        agent_one.followup(message("hi"));
        agent_one.when_idle().await;
        assert_eq!(
            systems.lock().unwrap().last().and_then(|s| s.as_deref()),
            Some("mock @ m1"),
            "first publish renders the initial route pair"
        );
        // /model switch → next execute republishes on the same tag with the
        // switched model (the old registration is still in the scoped layer;
        // registering again is a duplicate no-op).
        let agent_two = ReactLoopAgent::new(
            ctx,
            SessionId("a2".into()),
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("switched-model".into()),
                ..Default::default()
            },
            session,
            Arc::clone(&llm_view),
            tools,
            prompts,
        );
        agent_two.start();
        agent_two.followup(message("again"));
        agent_two.when_idle().await;
        assert_eq!(
            systems.lock().unwrap().last().and_then(|s| s.as_deref()),
            Some("mock @ switched-model"),
            "the next step's assembled system must contain the switched \
             model name — the variables read the current options at assembly"
        );
    }

    #[tokio::test]
    async fn instructions_snapshot_follows_claimed_input_and_resyncs() {
        use starriver_kernel::llm::ContentBlock;

        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("AGENTS.md"), "always use pnpm").unwrap();

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let systems = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let _adapter = llm
            .register_adapter(Arc::new(SystemCaptureAdapter {
                providers: vec!["mock".into()],
                systems,
                stream_chunks: SystemCaptureAdapter::stop_after_text(),
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let store = starriver_session::InMemorySessionStore::new(
            bus.clone(),
            Context::new().with_event_bus(&EventBus::new()),
        );
        let session = store
            .prepare(
                SessionId("a1".into()),
                starriver_kernel::session::PrepareOptions {
                    cwd: Some(ws.path().to_string_lossy().into_owned()),
                    seed: Vec::new(),
            agent_preset: None,
        },
            )
            .unwrap()
            .session
            .clone();

        let prompts = Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new());
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::clone(&prompts),
        );

        fn user_texts(agent: &ReactLoopAgent) -> Vec<String> {
            let events = agent.session().events();
            events
                .iter()
                .filter(|e| e.event_type == "user/message")
                .map(
                    |e| match Message::from_json(e.data.clone()).unwrap().content.first() {
                        Some(ContentBlock::Text(t)) => t.clone(),
                        other => format!("{other:?}"),
                    },
                )
                .collect()
        }

        agent.start();
        agent.followup(message("task one"));
        agent.when_idle().await;
        let seen = user_texts(&agent);
        assert_eq!(seen[0], "task one", "claimed input lands first");
        assert!(
            seen[1].contains("<system-reminder>") && seen[1].contains("always use pnpm"),
            "instructions baseline follows the claimed input: {:?}",
            seen.get(1)
        );
        assert_eq!(seen.len(), 2, "nothing else is projected on a quiet turn");

        // Unchanged workspace → no repeat of the reminder.
        agent.followup(message("task two"));
        agent.when_idle().await;
        assert_eq!(
            user_texts(&agent),
            vec![
                "task one".to_string(),
                seen[1].clone(),
                "task two".to_string()
            ],
            "change-only projection must not resend the reminder"
        );

        // Editing the workspace re-syncs: an updated reminder is projected once.
        std::fs::write(ws.path().join("STARRIVER.md"), "prefer rustfmt").unwrap();
        agent.followup(message("task three"));
        agent.when_idle().await;
        let final_seen = user_texts(&agent);
        assert_eq!(final_seen.len(), 5, "{final_seen:?}");
        assert_eq!(final_seen[3], "task three");
        assert!(
            final_seen[4].contains("prefer rustfmt") && final_seen[4].contains("always use pnpm"),
            "re-synced snapshot carries the union of instruction files: {:?}",
            final_seen[4]
        );
    }

    #[tokio::test]
    async fn multiple_complete_sections_end_the_turn_with_prompt_assemble() {
        use starriver_kernel::system_prompt::{SectionEntry, SectionText};
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let systems = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let _adapter = llm
            .register_adapter(Arc::new(SystemCaptureAdapter {
                providers: vec!["mock".into()],
                systems: Arc::clone(&systems),
                stream_chunks: SystemCaptureAdapter::stop_after_text(),
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let prompts = Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new());
        for name in ["mode:review", "mode:plan"] {
            prompts
                .section(
                    &ctx,
                    SectionEntry {
                        name: name.into(),
                        order: 80,
                        complete: true,
                        text: SectionText::Static(name.to_uppercase()).into(),
                    },
                )
                .unwrap();
        }
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::clone(&prompts),
        );
        agent.start();
        agent.followup(message("hi"));
        agent.when_idle().await;
        assert!(systems.lock().unwrap().is_empty(), "no request may escape");
        let events = agent.session().events();
        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("error")
        );
        let failure = format!("{:?}", turn_end.data);
        assert!(
            failure.contains("mode:review") && failure.contains("mode:plan"),
            "both conflicting complete sections must be named: {failure}"
        );
    }

    #[tokio::test]
    async fn restart_does_not_resend_instructions_snapshot() {
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("AGENTS.md"), "always use pnpm").unwrap();

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let _adapter = llm
            .register_adapter(Arc::new(SystemCaptureAdapter {
                providers: vec!["mock".into()],
                systems: Arc::new(Mutex::new(Vec::new())),
                stream_chunks: SystemCaptureAdapter::stop_after_text(),
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let store = starriver_session::InMemorySessionStore::new(
            bus.clone(),
            Context::new().with_event_bus(&EventBus::new()),
        );
        let session = store
            .prepare(
                SessionId("a1".into()),
                starriver_kernel::session::PrepareOptions {
                    cwd: Some(ws.path().to_string_lossy().into_owned()),
                    seed: Vec::new(),
            agent_preset: None,
        },
            )
            .unwrap()
            .session
            .clone();
        let prompts = Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new());

        let first = ReactLoopAgent::new(
            ctx.clone(),
            SessionId("a1".into()),
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session.clone(),
            Arc::clone(&llm) as Arc<dyn crate::services::LlmRuntimeView>,
            Arc::clone(&tools),
            Arc::clone(&prompts),
        );
        first.start();
        first.followup(message("turn one"));
        first.when_idle().await;

        // "Restart": a fresh agent over the same session history.
        let second = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            prompts,
        );
        second.start();
        second.followup(message("turn two"));
        second.when_idle().await;

        let events = second.session().events();
        let reminders: Vec<usize> = events
            .iter()
            .filter(|e| e.event_type == "user/message")
            .filter(|e| format!("{:?}", e.data).contains("<system-reminder>"))
            .map(|_| 1)
            .collect();
        assert_eq!(
            reminders.len(),
            1,
            "restart must restore digest state instead of resending the snapshot"
        );
    }

    #[tokio::test]
    async fn time_context_lands_last_and_throttles_within_window() {
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let _adapter = llm
            .register_adapter(Arc::new(SystemCaptureAdapter {
                providers: vec!["mock".into()],
                systems: Arc::new(Mutex::new(Vec::new())),
                stream_chunks: SystemCaptureAdapter::stop_after_text(),
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                // Effectively infinite window: exactly one sample.
                time_refresh_interval_ms: Some(u64::MAX),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        fn user_texts(agent: &ReactLoopAgent) -> Vec<String> {
            let events = agent.session().events();
            events
                .iter()
                .filter(|e| e.event_type == "user/message")
                .filter_map(
                    |e| match Message::from_json(e.data.clone()).ok()?.content.first() {
                        Some(ContentBlock::Text(t)) => Some(t.clone()),
                        _ => None,
                    },
                )
                .collect()
        }

        agent.start();
        agent.followup(message("one"));
        agent.when_idle().await;
        let seen = user_texts(&agent);
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[1].starts_with("Time sampled while preparing turn 1, step 1: "));
        assert!(
            seen[1].contains("Elapsed since the preceding model-visible message: 0s."),
            "elapsed measures the preceding model-visible message: {seen:?}"
        );

        agent.followup(message("two"));
        agent.when_idle().await;
        let seen = user_texts(&agent);
        assert_eq!(
            seen,
            vec!["one".to_string(), seen[1].clone(), "two".to_string()],
            "throttled second turn adds no time sample"
        );
    }

    #[tokio::test]
    async fn time_context_disabled_without_an_interval_option() {
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let _adapter = llm
            .register_adapter(Arc::new(SystemCaptureAdapter {
                providers: vec!["mock".into()],
                systems: Arc::new(Mutex::new(Vec::new())),
                stream_chunks: SystemCaptureAdapter::stop_after_text(),
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                time_refresh_interval_ms: None,
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        agent.start();
        agent.followup(message("one"));
        agent.when_idle().await;
        agent.followup(message("two"));
        agent.when_idle().await;

        let events = agent.session().events();
        let time_samples = events
            .iter()
            .filter(|e| e.event_type == "user/message")
            .filter(|e| format!("{:?}", e.data).contains("Time sampled while preparing"))
            .count();
        assert_eq!(
            time_samples, 0,
            "interval None must disable the time projector entirely"
        );
    }

    /// WI-163: the host-fed MCP availability snapshot must land as a
    /// model-visible notice exactly once per transition — healthy start and
    /// unchanged states stay silent, down and recovery each announce once.
    #[tokio::test]
    async fn mcp_status_notice_lands_once_per_transition() {
        use std::sync::Arc as StdArc;
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let _adapter = llm
            .register_adapter(Arc::new(SystemCaptureAdapter {
                providers: vec!["mock".into()],
                systems: Arc::new(Mutex::new(Vec::new())),
                stream_chunks: SystemCaptureAdapter::stop_after_text(),
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let unavailable: StdArc<Mutex<Vec<(String, String)>>> =
            StdArc::new(Mutex::new(Vec::new()));
        let feed_state = StdArc::clone(&unavailable);
        let session = Session::new(SessionId("a1".into()), bus);
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                mcp_status_feed: Some(
                    starriver_kernel::agent::SharedMcpStatusFeed(StdArc::new(move || {
                        feed_state.lock().unwrap().clone()
                    })),
                ),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        fn notices(agent: &ReactLoopAgent) -> Vec<String> {
            agent
                .session()
                .events()
                .iter()
                .filter(|e| e.event_type == "user/message")
                .filter_map(|e| Message::from_json(e.data.clone()).ok())
                .filter(|m| {
                    matches!(&m.source,
                        MessageSource::Plugin { plugin, .. }
                            if plugin == crate::mcp_status::MCP_STATUS_PLUGIN)
                })
                .map(|m| match m.content.first() {
                    Some(ContentBlock::Text(t)) => t.clone(),
                    other => format!("{other:?}"),
                })
                .collect()
        }

        // Healthy from the start: no notice.
        agent.start();
        agent.followup(message("one"));
        agent.when_idle().await;
        assert!(notices(&agent).is_empty(), "healthy start must stay silent");

        // Server goes down: exactly one notice naming it.
        unavailable
            .lock()
            .unwrap()
            .push(("nexplan".into(), "connection refused".into()));
        agent.followup(message("two"));
        agent.when_idle().await;
        let seen = notices(&agent);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].contains("- nexplan: connection refused"), "{seen:?}");
        assert!(seen[0].contains("不要用其他工具模拟"), "{seen:?}");

        // Still down on the next turn: deduplicated.
        agent.followup(message("three"));
        agent.when_idle().await;
        assert_eq!(notices(&agent).len(), 1, "unchanged state re-announces");

        // Recovers: one recovery notice, then steady silence.
        unavailable.lock().unwrap().clear();
        agent.followup(message("four"));
        agent.when_idle().await;
        let seen = notices(&agent);
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[1].contains("已恢复"), "{seen:?}");
        agent.followup(message("five"));
        agent.when_idle().await;
        assert_eq!(notices(&agent).len(), 2, "recovery is announced once");
    }

    #[tokio::test]
    async fn followup_queues_and_wakes_driver() {
        let agent = agent_arc();
        agent.start();
        assert_eq!(agent.status(), AgentStatus::Idle);

        agent.followup(message("hi"));
        agent.when_idle().await;

        assert_eq!(agent.status(), AgentStatus::Idle);
        let inbox = agent.inbox.lock().unwrap();
        // The driver claimed the prompt into a turn; the queue is drained.
        assert_eq!(inbox.next_turn().len(), 0);
    }

    #[tokio::test]
    async fn pending_remove_and_update_text_edit_the_queue() {
        let agent = agent_arc();
        agent.start();
        agent.send(message("one"), InboxTarget::NextTurn, false);
        agent.send(message("two"), InboxTarget::NextTurn, false);
        assert_eq!(agent.pending_messages().next_turn.len(), 2);

        assert!(agent.pending_update_text("one", "edited"));
        let pending = agent.pending_messages().next_turn;
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].id.0, "one");
        assert_eq!(pending[0].content.len(), 1);
        assert!(matches!(&pending[0].content[0], ContentBlock::Text(t) if t == "edited"));

        assert!(agent.pending_remove("two"));
        assert_eq!(agent.pending_messages().next_turn.len(), 1);
        assert!(!agent.pending_remove("missing"));
        assert!(!agent.pending_update_text("missing", "x"));
    }

    #[tokio::test]
    async fn pending_replace_swaps_content_but_keeps_identity() {
        let agent = agent_arc();
        agent.start();
        agent.send(message("one"), InboxTarget::NextTurn, false);

        // The host rebuilds edited queue text into Text/Image blocks after
        // image admission; the replace must keep the message id (the dock
        // row's stable key) and role.
        let replaced = agent.pending_replace(
            "one",
            vec![
                ContentBlock::Text("edited".into()),
                ContentBlock::Image {
                    attachment: starriver_kernel::json::JsonValue::Object(vec![(
                        "fileId".into(),
                        starriver_kernel::json::JsonValue::String("sha256:abc".into()),
                    )]),
                },
            ],
        );
        assert!(replaced);
        let pending = agent.pending_messages().next_turn;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id.0, "one");
        assert_eq!(pending[0].role, MessageRole::User);
        assert_eq!(pending[0].content.len(), 2);
        assert!(matches!(&pending[0].content[0], ContentBlock::Text(t) if t == "edited"));
        assert!(matches!(&pending[0].content[1], ContentBlock::Image { .. }));
        assert!(!agent.pending_replace("missing", vec![ContentBlock::Text("x".into())]));
    }

    #[tokio::test]
    async fn cancel_with_inbox_parks_until_pending_flush() {
        let agent = agent_arc();
        agent.start();
        agent.send(message("queued"), InboxTarget::NextTurn, false);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(agent.status(), AgentStatus::Idle);
        assert_eq!(agent.pending_messages().next_turn.len(), 1);

        agent.cancel(AgentCancelCause::new(AgentCancelKind::User), true);
        agent.pending_flush();
        agent.when_idle().await;
        assert!(agent.pending_messages().next_turn.is_empty());
    }

    #[tokio::test]
    async fn parked_driver_does_not_auto_continue_after_mid_turn_abort() {
        struct OneShotHangAdapter {
            hang: Arc<std::sync::atomic::AtomicBool>,
            providers: Vec<String>,
        }
        impl starriver_kernel::llm::LlmAdapter for OneShotHangAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                use starriver_kernel::llm::{FinishReason, StreamChunk};
                use futures::stream;
                if self.hang.load(std::sync::atomic::Ordering::SeqCst) {
                    Box::pin(stream::pending())
                } else {
                    Box::pin(stream::iter(vec![Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    })]))
                }
            }
        }

        let hang = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let _adapter = llm
            .register_adapter(Arc::new(OneShotHangAdapter {
                hang: hang.clone(),
                providers: vec!["mock".into()],
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn ToolRuntimeView>;
        let agent = Arc::new(ReactLoopAgent::new(
            ctx,
            SessionId("a2".into()),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            Session::new(SessionId("a2".into()), bus),
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        ));
        agent.start();

        agent.followup(message("running"));
        let mut waited = 0;
        while agent.status() != AgentStatus::Running && waited < 2000 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            waited += 10;
        }
        assert_eq!(agent.status(), AgentStatus::Running);

        agent.send(message("queued"), InboxTarget::NextTurn, false);
        assert!(agent.cancel(AgentCancelCause::new(AgentCancelKind::User), true));
        hang.store(false, std::sync::atomic::Ordering::SeqCst);
        agent.when_idle().await;
        eprintln!("EVENTS: {:?}", agent.session().events().iter().filter(|e| e.event_type == "turn/end" || e.event_type == "agent/inbox/spliced").map(|e| (e.event_type.clone(), e.data.clone())).collect::<Vec<_>>());

        let pending = agent.pending_messages().next_turn;
        assert_eq!(pending.len(), 1, "parked queue must not auto-continue");
        assert_eq!(pending[0].id.0, message("queued").id.0);

        eprintln!("EVENTS: {:?}", agent.session().events().iter().map(|e| (e.event_type.clone(), e.data.as_str().map(|s| s.chars().take(160).collect::<String>()))).collect::<Vec<_>>());
        agent.pending_flush();
        agent.when_idle().await;
        assert!(agent.pending_messages().next_turn.is_empty());
    }

    #[tokio::test]
    async fn inject_does_not_wake_idle_driver() {
        let agent = agent_arc();
        agent.start();
        agent.inject(message("ctx"));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(agent.status(), AgentStatus::Idle);
        let inbox = agent.inbox.lock().unwrap();
        assert_eq!(inbox.next_step().len(), 1);
    }

    #[tokio::test]
    async fn cancel_on_idle_is_noop() {
        let agent = agent_arc();
        agent.start();
        // Idle cancel is a no-op and now says so: callers reset their
        // "stopping" affordance instead of awaiting a TaskCancelled that
        // never arrives.
        assert!(!agent.cancel(AgentCancelCause::new(AgentCancelKind::User), false));
        assert_eq!(agent.status(), AgentStatus::Idle);
    }

    #[tokio::test]
    async fn single_followup_produces_turn_and_step_boundaries() {
        use futures::stream;
        use starriver_kernel::llm::{LlmAdapter, LlmRuntime, StreamChunk};

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                // Real providers always terminate with a finish reason —
                // an empty stream without one is now a truncation error
                // (BUG-99), so the mock must model the terminal chunk.
                Box::pin(stream::iter(vec![Ok(
                    StreamChunk::Finish {
                        reason: starriver_kernel::llm::FinishReason::Stop,
                        replay_state: None,
                    },
                )]))
            }
        }

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let _adapter = llm
            .register_adapter(Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        agent.start();
        agent.followup(message("hi"));
        agent.when_idle().await;

        let events = agent.session().events();
        let types = events
            .iter()
            .map(|e| e.event_type.as_str())
            .collect::<Vec<_>>();
        assert!(types.contains(&"turn/start"));
        assert!(types.contains(&"turn/end"));
        assert!(types.contains(&"step/start"));
        assert!(types.contains(&"step/end"));
        assert!(types.contains(&"user/message"));

        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|reason| reason.as_str_at("kind")),
            Some("completed")
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "user/message")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .find(|e| e.event_type == "user/message")
                .and_then(|e| e.data.as_str_at("role")),
            Some("user")
        );
    }

    #[tokio::test]
    async fn step_streams_chunks_and_appends_assistant_message() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                Box::pin(stream::iter(vec![
                    Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "hello".into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::Text("hello".into()),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]))
            }
        }

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let _adapter = llm
            .register_adapter(Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        agent.start();
        agent.followup(message("hi"));
        agent.when_idle().await;

        let events = agent.session().events();
        assert!(events.iter().any(|e| e.event_type == "assistant/chunk"));
        let assistant = events
            .iter()
            .find(|e| e.event_type == "assistant/message")
            .expect("assistant/message");
        assert!(assistant.surface_op.is_some());
        assert_eq!(assistant.source_event_seqs.len(), 3);
        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn cancel_mid_stream_appends_interrupted_anchor() {
        use futures::stream;
        use starriver_kernel::llm::{LlmAdapter, LlmError, StreamChunk};

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                let cancellation = options.cancellation.clone();
                Box::pin(
                    stream::iter(vec![Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "partial".into(),
                    })])
                    .chain(stream::once(async move {
                        if let Some(token) = cancellation {
                            token.cancelled().await;
                        }
                        Err(LlmError::Cancelled)
                    })),
                )
            }
        }

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let _adapter = llm
            .register_adapter(Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        agent.start();
        agent.followup(message("hi"));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        agent.cancel(
            starriver_kernel::agent::AgentCancelCause::new(
                starriver_kernel::agent::AgentCancelKind::User,
            ),
            false,
        );
        agent.when_idle().await;

        let events = agent.session().events();
        let anchor = events
            .iter()
            .find(|e| e.event_type == "assistant/message")
            .expect("interrupted anchor");
        assert_eq!(
            anchor
                .data
                .get("interrupted")
                .and_then(starriver_kernel::json::JsonValue::as_bool),
            Some(true)
        );
        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("aborted")
        );
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("cause")),
            Some("user")
        );
    }

    #[tokio::test]
    async fn request_error_retry_recovers() {
        use futures::stream;
        use starriver_kernel::agent::{AgentRequestError, RequestErrorAction};
        use starriver_kernel::llm::{FinishReason, LlmAdapter, LlmError, LlmRuntime, StreamChunk};
        use std::sync::atomic::{AtomicBool, Ordering};

        struct MockAdapter {
            providers: Vec<String>,
            first: AtomicBool,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                if self.first.swap(false, Ordering::SeqCst) {
                    Box::pin(stream::iter(vec![Err(LlmError::Call {
                        code: "RATE_LIMIT".into(),
                        message: "slow down".into(),
                        status: Some(429),
                        provider_retry_after_ms: None,
                        request_id: None,
                    })]))
                } else {
                    Box::pin(stream::iter(vec![
                        Ok(StreamChunk::TextDelta {
                            index: 0,
                            text: "recovered".into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index: 0,
                            block: starriver_kernel::llm::ContentBlock::Text("recovered".into()),
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::Stop,
                            replay_state: None,
                        }),
                    ]))
                }
            }
        }

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let _adapter = llm
            .register_adapter(Arc::new(MockAdapter {
                providers: vec!["mock".into()],
                first: AtomicBool::new(true),
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        let _retry = agent
            .ctx()
            .on_waterfall::<AgentRequestError, _>(|_, _next| {
                Box::pin(async { Ok(RequestErrorAction::Retry) })
            });
        agent.start();
        agent.followup(message("hi"));
        agent.when_idle().await;

        let events = agent.session().events();
        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("completed")
        );
        assert!(events.iter().any(|e| e.event_type == "assistant/message"));
    }

    #[tokio::test]
    async fn error_emits_agent_error_event() {
        use futures::stream;
        use starriver_kernel::agent::AgentErrorEvent;
        use starriver_kernel::llm::{LlmAdapter, LlmError, LlmRuntime};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<starriver_kernel::llm::StreamChunk, LlmError>,
            > {
                Box::pin(stream::iter(vec![Err(LlmError::Call {
                    // AUTH is non-retryable: the turn must die on the first
                    // failure (SERVER would now burn the retry budget first —
                    // see retry_transient_failure — and sleep through the
                    // backoff ladder for no diagnostic gain here).
                    code: "AUTH".into(),
                    message: "boom".into(),
                    status: Some(401),
                    provider_retry_after_ms: None,
                    request_id: None,
                })]))
            }
        }

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let _adapter = llm
            .register_adapter(Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let _listener = agent.ctx().on::<AgentErrorEvent, _>(move |_| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(cordis_rslite::Value::new(()))
            })
        });
        agent.start();
        agent.followup(message("hi"));
        agent.when_idle().await;

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        let events = agent.session().events();
        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("error")
        );
    }

    /// BUG-106: after a turn ends in a fast LLM failure the driver must go
    /// quiet. The outer `start()` loop must not recycle its wake permit
    /// forever (acquire → guard drop → acquire): that livelock emits
    /// AgentStatusChanged pairs at full speed and burns a core until the
    /// process exits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn driver_quiesces_after_fast_llm_failure() {
        use futures::stream;
        use starriver_kernel::agent::AgentStatusChanged;
        use starriver_kernel::llm::{LlmAdapter, LlmError, LlmRuntime};
        use std::sync::atomic::AtomicUsize;

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<starriver_kernel::llm::StreamChunk, LlmError>,
            > {
                Box::pin(stream::iter(vec![Err(LlmError::Call {
                    code: "INVALID_REQUEST".into(),
                    message: "model not found".into(),
                    status: Some(400),
                    provider_retry_after_ms: None,
                    request_id: None,
                })]))
            }
        }

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let _adapter = llm
            .register_adapter(Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        let status_flips = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&status_flips);
        let _listener = agent.ctx().on::<AgentStatusChanged, _>(move |_| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(cordis_rslite::Value::new(()))
            })
        });
        agent.start();
        agent.followup(message("hi"));
        agent.when_idle().await;

        // The driver is idle: let the turn's own async status listeners
        // drain, then over a generous window no further flips may arrive.
        // A livelocked outer loop emits thousands.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let baseline = status_flips.load(Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let after = status_flips.load(Ordering::SeqCst);
        assert_eq!(
            after, baseline,
            "driver still cycling after a fast LLM failure: {baseline} -> {after}"
        );
    }

    #[tokio::test]
    async fn cancel_then_followup_starts_fresh_turn() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, LlmError, LlmRuntime, StreamChunk};
        use std::sync::atomic::{AtomicBool, Ordering};

        struct MockAdapter {
            providers: Vec<String>,
            first: AtomicBool,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                if self.first.swap(false, Ordering::SeqCst) {
                    let cancellation = options.cancellation.clone();
                    Box::pin(
                        stream::iter(vec![Ok(StreamChunk::TextDelta {
                            index: 0,
                            text: "partial".into(),
                        })])
                        .chain(stream::once(async move {
                            if let Some(token) = cancellation {
                                token.cancelled().await;
                            }
                            Err(LlmError::Cancelled)
                        })),
                    )
                } else {
                    Box::pin(stream::iter(vec![
                        Ok(StreamChunk::TextDelta {
                            index: 0,
                            text: "done".into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index: 0,
                            block: starriver_kernel::llm::ContentBlock::Text("done".into()),
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::Stop,
                            replay_state: None,
                        }),
                    ]))
                }
            }
        }

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let _adapter = llm
            .register_adapter(Arc::new(MockAdapter {
                providers: vec!["mock".into()],
                first: AtomicBool::new(true),
            }))
            .unwrap();
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        agent.start();
        agent.followup(message("a"));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        agent.followup(message("b"));
        agent.cancel(
            starriver_kernel::agent::AgentCancelCause::new(
                starriver_kernel::agent::AgentCancelKind::User,
            ),
            true,
        );
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(
            idle.is_ok(),
            "driver must converge after cancel + pending followup"
        );

        let events = agent.session().events();
        let turn_starts = events
            .iter()
            .filter(|e| e.event_type == "turn/start")
            .count();
        assert_eq!(turn_starts, 1);
        assert_eq!(agent.pending_messages().next_turn.len(), 1);

        agent.followup(message("c"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(
            idle.is_ok(),
            "waking send must resume the parked queue FIFO"
        );
        let events = agent.session().events();
        let turn_starts = events
            .iter()
            .filter(|e| e.event_type == "turn/start")
            .count();
        assert_eq!(turn_starts, 3);
        assert_eq!(agent.pending_messages().next_turn.len(), 0);
        let last_turn_end = events
            .iter()
            .rev()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            last_turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn scheduler_failure_ends_turn_with_error() {
        use futures::stream;
        use starriver_kernel::llm::{
            CallId, FinishReason, LlmAdapter, LlmRuntime, StreamChunk, ToolSchema,
        };
        use starriver_kernel::tools::{
            ToolError, ToolExecutionInput, ToolExecutionMode, ToolExecutionResult, ToolRunContext,
        };

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                Box::pin(stream::iter(vec![
                    Ok(StreamChunk::ToolCallDelta {
                        index: 0,
                        id: CallId("c1".into()),
                        name: Some("echo".into()),
                        arguments_delta: "{}".into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::ToolCall {
                            id: CallId("c1".into()),
                            name: Some("echo".into()),
                            arguments: JsonValue::Object(Vec::new()),
                        },
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::ToolCalls,
                        replay_state: None,
                    }),
                ]))
            }
        }

        struct FailingTools;
        impl crate::services::ToolRuntimeView for FailingTools {
            fn schemas(&self) -> Vec<ToolSchema> {
                Vec::new()
            }
            fn schemas_for(&self, _agent_id: Option<&str>) -> Vec<ToolSchema> {
                Vec::new()
            }
            fn sdk_schemas(&self) -> Vec<ToolSchema> {
                Vec::new()
            }
            fn is_ptc_active(&self) -> bool {
                false
            }
            fn restrict_for(
                &self,
                _agent_id: &str,
                _allow: Vec<String>,
            ) -> Result<cordis_rslite::EffectHandle, ToolError> {
                Ok(cordis_rslite::EffectHandle::none())
            }
            fn execution_mode(&self, _input: &ToolExecutionInput) -> ToolExecutionMode {
                ToolExecutionMode::Exclusive
            }
            fn prepare(
                &self,
                _input: ToolExecutionInput,
            ) -> cordis_rslite::BoxFuture<
                Result<starriver_kernel::tools::ScheduledToolPreparation, ToolError>,
            > {
                Box::pin(async { Err(ToolError::NotFound("boom".into())) })
            }
            fn dispatch(
                &self,
                _exec: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<
                Result<starriver_kernel::tools::ScheduledToolDispatch, ToolError>,
            > {
                Box::pin(async { Err(ToolError::NotFound("unused".into())) })
            }
            fn finalize(
                &self,
                _exec: ToolRunContext,
                _result: ToolExecutionResult,
            ) -> cordis_rslite::BoxFuture<Result<ToolExecutionResult, ToolError>> {
                Box::pin(async { Err(ToolError::NotFound("unused".into())) })
            }
            fn finish(
                &self,
                _exec: ToolRunContext,
                _result: ToolExecutionResult,
            ) -> Result<ToolExecutionResult, ToolError> {
                Err(ToolError::NotFound("unused".into()))
            }

            fn presentation_meta(
                &self,
                _input: &starriver_kernel::tools::ToolExecutionInput,
                _value: &cordis_rslite::Value,
            ) -> Option<serde_json::Value> {
                None
            }
        }

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let _adapter = llm
            .register_adapter(Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }))
            .unwrap();
        let tools = Arc::new(FailingTools) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        agent.start();
        agent.followup(message("hi"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok(), "driver must converge after scheduler failure");

        let events = agent.session().events();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "tool/result")
                .count(),
            0,
            "scheduler failure must not fabricate tool results"
        );
        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("error")
        );
    }

    #[tokio::test]
    async fn driver_panic_is_contained_and_agent_survives() {
        use futures::stream;
        use starriver_kernel::agent::AgentErrorEvent;
        use starriver_kernel::llm::{
            FinishReason, GenerateOptions, LlmCallConfig, LlmError, PreparedLlmCall, StreamChunk,
        };
        use std::sync::atomic::{AtomicBool, Ordering};

        struct PanicLlm {
            first: AtomicBool,
        }
        impl crate::services::LlmRuntimeView for PanicLlm {
            fn prepare_call(
                &self,
                _config: LlmCallConfig,
                _cancellation: Option<CancellationToken>,
            ) -> cordis_rslite::BoxFuture<Result<PreparedLlmCall, LlmError>> {
                Box::pin(async { Err(LlmError::NoAdapter("mock".into())) })
            }
            fn stream(
                &self,
                _options: GenerateOptions,
            ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
                if self.first.swap(false, Ordering::SeqCst) {
                    Box::pin(stream::once(async { panic!("driver panic") }))
                } else {
                    Box::pin(stream::iter(vec![
                        Ok(StreamChunk::TextDelta {
                            index: 0,
                            text: "ok".into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index: 0,
                            block: starriver_kernel::llm::ContentBlock::Text("ok".into()),
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::Stop,
                            replay_state: None,
                        }),
                    ]))
                }
            }
        }

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(PanicLlm {
            first: AtomicBool::new(true),
        }) as Arc<dyn crate::services::LlmRuntimeView>;
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let _listener = agent.ctx().on::<AgentErrorEvent, _>(move |_| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(cordis_rslite::Value::new(()))
            })
        });
        agent.start();

        agent.followup(message("a"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok(), "driver must survive a panic");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(seen.load(Ordering::SeqCst), 1, "panic must be reported");

        // The persistent driver is still alive: a second turn completes.
        agent.followup(message("b"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok(), "second turn must run after the panic");
        let events = agent.session().events();
        let last_turn_end = events
            .iter()
            .rev()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            last_turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("completed")
        );
    }

    /// Build an agent with a scripted adapter and a tools view.
    fn agent_with(
        adapter: Arc<dyn starriver_kernel::llm::LlmAdapter>,
        tools: Arc<dyn crate::services::ToolRuntimeView>,
    ) -> Arc<ReactLoopAgent> {
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
        let _adapter = llm.register_adapter(adapter).unwrap();
        let session = Session::new(SessionId("a1".into()), bus);
        ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        )
    }

    #[tokio::test]
    async fn sequential_turns_complete_independently() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct MockAdapter {
            providers: Vec<String>,
            calls: AtomicUsize,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                let text = format!("reply-{call}");
                Box::pin(stream::iter(vec![
                    Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: text.clone(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::Text(text),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]))
            }
        }

        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
                calls: AtomicUsize::new(0),
            }),
            tools,
        );
        agent.start();
        agent.followup(message("a"));
        agent.followup(message("b"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok());

        let events = agent.session().events();
        let turn_starts = events
            .iter()
            .filter(|e| e.event_type == "turn/start")
            .count();
        assert_eq!(turn_starts, 2);
        let completed = events
            .iter()
            .filter(|e| {
                e.event_type == "turn/end"
                    && e.data.get("reason").and_then(|r| r.as_str_at("kind")) == Some("completed")
            })
            .count();
        assert_eq!(completed, 2);
    }

    #[tokio::test]
    async fn cancel_without_keep_inbox_drains_pending() {
        use futures::stream;
        use starriver_kernel::llm::{LlmAdapter, LlmError, StreamChunk};

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                let cancellation = options.cancellation.clone();
                Box::pin(
                    stream::iter(vec![Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "partial".into(),
                    })])
                    .chain(stream::once(async move {
                        if let Some(token) = cancellation {
                            token.cancelled().await;
                        }
                        Err(LlmError::Cancelled)
                    })),
                )
            }
        }

        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        agent.start();
        agent.followup(message("a"));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        agent.followup(message("b"));
        agent.cancel(
            starriver_kernel::agent::AgentCancelCause::new(
                starriver_kernel::agent::AgentCancelKind::User,
            ),
            false,
        );
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok());

        let events = agent.session().events();
        let turn_starts = events
            .iter()
            .filter(|e| e.event_type == "turn/start")
            .count();
        assert_eq!(
            turn_starts, 1,
            "queued turn must be cancelled with the inbox"
        );
        assert!(!agent.inbox.lock().unwrap().has_pending());
    }

    #[tokio::test]
    async fn steer_after_abort_runs_in_next_turn() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, LlmError, StreamChunk};

        struct MockAdapter {
            providers: Vec<String>,
            first: std::sync::atomic::AtomicBool,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                if self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    let cancellation = options.cancellation.clone();
                    Box::pin(
                        stream::iter(vec![Ok(StreamChunk::TextDelta {
                            index: 0,
                            text: "partial".into(),
                        })])
                        .chain(stream::once(async move {
                            if let Some(token) = cancellation {
                                token.cancelled().await;
                            }
                            Err(LlmError::Cancelled)
                        })),
                    )
                } else {
                    Box::pin(stream::iter(vec![
                        Ok(StreamChunk::TextDelta {
                            index: 0,
                            text: "done".into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index: 0,
                            block: starriver_kernel::llm::ContentBlock::Text("done".into()),
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::Stop,
                            replay_state: None,
                        }),
                    ]))
                }
            }
        }

        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
                first: std::sync::atomic::AtomicBool::new(true),
            }),
            tools,
        );
        agent.start();
        agent.followup(message("a"));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        agent.steer(message("steer"));
        agent.cancel(
            starriver_kernel::agent::AgentCancelCause::new(
                starriver_kernel::agent::AgentCancelKind::User,
            ),
            true,
        );
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok());

        let events = agent.session().events();
        let turn_starts = events
            .iter()
            .filter(|e| e.event_type == "turn/start")
            .count();
        assert_eq!(turn_starts, 1);
        assert_eq!(agent.pending_messages().next_step.len(), 1);

        agent.followup(message("c"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok());
        let events = agent.session().events();
        let steer_consumed = events
            .iter()
            .filter(|e| e.event_type == "user/message")
            .any(|e| e.data.as_str_at("id") == Some("steer"));
        assert!(steer_consumed, "steer must be consumed by the next turn");
        assert!(!agent.inbox.lock().unwrap().has_pending());
    }

    #[tokio::test]
    async fn cancel_during_tool_step_synthesizes_skipped_result() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};
        use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

        struct SlowTool;
        impl ToolExecutor for SlowTool {
            fn execute(
                &self,
                arguments: cordis_rslite::Value,
                ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                Box::pin(async move {
                    if let Some(token) = ctx.cancellation() {
                        tokio::select! {
                            _ = token.cancelled() => {}
                            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
                        }
                    } else {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    Ok(arguments)
                })
            }
        }

        fn slow_definition(name: &str) -> ToolDefinition {
            ToolDefinition {
                requires_approval: false,
                deny_in_readonly: false,
                name: name.into(),
                description: "slow".into(),
                parameters: cordis_rslite::Value::new(()),
                output: cordis_rslite::Value::new(()),
                render: Arc::new(|_, value| {
                    vec![starriver_kernel::llm::ContentBlock::Text(format!(
                        "{value:?}"
                    ))]
                }),
                executor: Arc::new(SlowTool),
                is_concurrency_safe: None,
                timeout: None,
                finalize_content: None,
            }
        }

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                let block = starriver_kernel::llm::ContentBlock::ToolCall {
                    id: CallId("c1".into()),
                    name: Some("slow".into()),
                    arguments: JsonValue::Object(Vec::new()),
                };
                let second = starriver_kernel::llm::ContentBlock::ToolCall {
                    id: CallId("c2".into()),
                    name: Some("slow".into()),
                    arguments: JsonValue::Object(Vec::new()),
                };
                Box::pin(stream::iter(vec![
                    Ok(StreamChunk::ToolCallDelta {
                        index: 0,
                        id: CallId("c1".into()),
                        name: Some("slow".into()),
                        arguments_delta: "{}".into(),
                    }),
                    Ok(StreamChunk::BlockEnd { index: 0, block }),
                    Ok(StreamChunk::ToolCallDelta {
                        index: 1,
                        id: CallId("c2".into()),
                        name: Some("slow".into()),
                        arguments_delta: "{}".into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 1,
                        block: second,
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::ToolCalls,
                        replay_state: None,
                    }),
                ]))
            }
        }

        let tools_runtime =
            starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _h1 = tools_runtime.register(slow_definition("slow")).unwrap();
        let tools = Arc::new(tools_runtime) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        agent.start();
        agent.followup(message("a"));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        agent.cancel(
            starriver_kernel::agent::AgentCancelCause::new(
                starriver_kernel::agent::AgentCancelKind::User,
            ),
            false,
        );
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok(), "cancel during tool step must converge");

        let events = agent.session().events();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "tool/call")
                .count(),
            2
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "tool/result")
                .count(),
            2,
            "started call + synthetic skipped call must both be recorded"
        );
        let last_result = events
            .iter()
            .rfind(|e| e.event_type == "tool/result")
            .unwrap();
        assert_eq!(
            last_result
                .data
                .get("error")
                .and_then(|e| e.as_str_at("code")),
            Some("ABORTED_BEFORE_DISPATCH"),
            "the undispatched second call must carry the synthetic code"
        );
        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("aborted")
        );
    }

    /// BUG-114: within one turn the executor sees each reload_image file id
    /// at most once — repeats (same step or later steps) are answered inline
    /// with the stop hint instead of another "re-attached" success, calls
    /// interleaved between the repeats still run through the scheduler, and
    /// the turn still completes.
    #[tokio::test]
    async fn duplicate_reload_image_calls_are_short_circuited() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};
        use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Counting {
            name: &'static str,
            executions: Arc<AtomicUsize>,
        }
        impl ToolExecutor for Counting {
            fn execute(
                &self,
                _arguments: cordis_rslite::Value,
                _ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                let executions = Arc::clone(&self.executions);
                let name = self.name;
                Box::pin(async move {
                    executions.fetch_add(1, Ordering::SeqCst);
                    Ok(cordis_rslite::Value::new(
                        serde_json::json!({ "output": format!("{name} ok") }),
                    ))
                })
            }
        }
        let counting = |name: &'static str| {
            let executions = Arc::new(AtomicUsize::new(0));
            let definition = ToolDefinition {
                requires_approval: false,
                deny_in_readonly: false,
                name: name.into(),
                description: format!("{name} stub").into(),
                parameters: cordis_rslite::Value::new(()),
                output: cordis_rslite::Value::new(()),
                render: Arc::new(move |_, _| {
                    vec![starriver_kernel::llm::ContentBlock::Text(format!(
                        "{name} ok"
                    ))]
                }),
                executor: Arc::new(Counting {
                    name,
                    executions: Arc::clone(&executions),
                }),
                is_concurrency_safe: None,
                timeout: None,
                finalize_content: None,
            };
            (definition, executions)
        };
        let (reload_definition, reload_executions) = counting("reload_image");
        let (echo_definition, echo_executions) = counting("echo");

        /// Phases keyed on settled tool results (noop pairs count too):
        /// 0 → first pull; 1 → echo, same-id repeat, echo (segmentation
        /// around the duplicate); 4 → cross-step repeat; 5+ → done.
        struct ReloadStormAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for ReloadStormAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                let settled = options
                    .messages
                    .iter()
                    .flat_map(|m| m.content.iter())
                    .filter(|b| {
                        matches!(b, starriver_kernel::llm::ContentBlock::ToolResult { .. })
                    })
                    .count();
                let call = |id: &str, name: &str, arguments: &str| {
                    starriver_kernel::llm::ContentBlock::ToolCall {
                        id: CallId(id.into()),
                        name: Some(name.into()),
                        arguments: JsonValue::from_json_str(arguments).unwrap(),
                    }
                };
                const RELOAD_ARGS: &str = r#"{"file_id":"sha256:dup"}"#;
                let blocks: Vec<starriver_kernel::llm::ContentBlock> = match settled {
                    0 => vec![call("r1", "reload_image", RELOAD_ARGS)],
                    1 => vec![
                        call("e1", "echo", "{}"),
                        call("r2", "reload_image", RELOAD_ARGS),
                        call("e2", "echo", "{}"),
                    ],
                    4 => vec![call("r3", "reload_image", RELOAD_ARGS)],
                    _ => vec![starriver_kernel::llm::ContentBlock::Text("done".into())],
                };
                let chunks: Vec<Result<StreamChunk, _>> = blocks
                    .iter()
                    .enumerate()
                    .flat_map(|(index, block)| {
                        let index = index as u64;
                        let deltas = match block {
                            starriver_kernel::llm::ContentBlock::ToolCall { id, name, .. } => {
                                let arguments_delta = if name.as_deref() == Some("reload_image") {
                                    RELOAD_ARGS.to_string()
                                } else {
                                    "{}".to_string()
                                };
                                vec![Ok(StreamChunk::ToolCallDelta {
                                    index,
                                    id: id.clone(),
                                    name: name.clone(),
                                    arguments_delta,
                                })]
                            }
                            _ => vec![Ok(StreamChunk::TextDelta {
                                index,
                                text: "done".into(),
                            })],
                        };
                        deltas.into_iter().chain(std::iter::once(Ok(StreamChunk::BlockEnd {
                            index,
                            block: block.clone(),
                        })))
                    })
                    .collect();
                let finish_reason = if matches!(settled, 0 | 1 | 4) {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                };
                let mut chunks = chunks;
                chunks.push(Ok(StreamChunk::Finish {
                    reason: finish_reason,
                    replay_state: None,
                }));
                Box::pin(stream::iter(chunks))
            }
        }

        let tools_runtime =
            starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _h1 = tools_runtime.register(reload_definition).unwrap();
        let _h2 = tools_runtime.register(echo_definition).unwrap();
        let tools = Arc::new(tools_runtime) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(ReloadStormAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        agent.start();
        agent.followup(message("reload storm"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok(), "turn must converge");

        // The reload id executed exactly once (r1); r2 (same step) and r3
        // (later step) never reached the executor.
        assert_eq!(reload_executions.load(Ordering::SeqCst), 1);
        // The calls interleaved around the duplicate still ran through the
        // scheduler in model order.
        assert_eq!(echo_executions.load(Ordering::SeqCst), 2);
        let events = agent.session().events();
        let reload_calls = events
            .iter()
            .filter(
                |e| e.event_type == "tool/call" && e.data.as_str_at("name") == Some("reload_image"),
            )
            .count();
        assert_eq!(reload_calls, 3, "every model call stays recorded");
        let noop_results = events
            .iter()
            .filter(|e| {
                e.event_type == "tool/result"
                    && format!("{:?}", e.data.get("message").unwrap()).contains("无需重复调用")
            })
            .count();
        assert_eq!(noop_results, 2, "r2 and r3 are answered inline");
        for event in events.iter().filter(|e| e.event_type == "tool/result") {
            assert!(
                event.data.get("error").is_none(),
                "no result may be flagged as an error"
            );
        }
        // Model order survives the segmentation: e1 → r2 → e2.
        let call_order: Vec<&str> = events
            .iter()
            .filter(|e| e.event_type == "tool/call")
            .filter_map(|e| e.data.as_str_at("callId"))
            .collect();
        let expected = ["r1", "e1", "r2", "e2", "r3"];
        assert!(
            call_order == expected,
            "call order must stay faithful to the assistant message: {call_order:?}"
        );
    }

    /// BUG-115: a reload that FAILS (NOT_FOUND) must neither pin its id nor
    /// turn a repeat into a lying "already re-attached" — the repeat gets
    /// the failure-aware stop hint, and the executor still ran only once.
    #[tokio::test]
    async fn failed_reload_does_not_pin_and_repeat_gets_failure_hint() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};
        use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct FailingReload {
            executions: Arc<AtomicUsize>,
        }
        impl ToolExecutor for FailingReload {
            fn execute(
                &self,
                _arguments: cordis_rslite::Value,
                _ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                let executions = Arc::clone(&self.executions);
                Box::pin(async move {
                    executions.fetch_add(1, Ordering::SeqCst);
                    Err(ToolError::Execution {
                        tool: "reload_image".into(),
                        code: "NOT_FOUND".into(),
                        message: "attachment unavailable".into(),
                    })
                })
            }
        }
        let executions = Arc::new(AtomicUsize::new(0));
        let definition = ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: "reload_image".into(),
            description: "stub reload".into(),
            parameters: cordis_rslite::Value::new(()),
            output: cordis_rslite::Value::new(()),
            render: Arc::new(|_, _| Vec::new()),
            executor: Arc::new(FailingReload {
                executions: Arc::clone(&executions),
            }),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        };

        /// 0 settled results → first (failing) reload; 1 → the REPEAT with
        /// the same id (short-circuited to a failure hint); 2+ → done.
        struct FailingAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for FailingAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                let settled = options
                    .messages
                    .iter()
                    .flat_map(|m| m.content.iter())
                    .filter(|b| {
                        matches!(b, starriver_kernel::llm::ContentBlock::ToolResult { .. })
                    })
                    .count();
                let done = settled >= 2;
                let block = if done {
                    starriver_kernel::llm::ContentBlock::Text("done".into())
                } else {
                    starriver_kernel::llm::ContentBlock::ToolCall {
                        id: CallId(format!("f{settled}")),
                        name: Some("reload_image".into()),
                        arguments: JsonValue::from_json_str(
                            r#"{"file_id":"sha256:gone"}"#,
                        )
                        .unwrap(),
                    }
                };
                let index = 0u64;
                let chunks: Vec<Result<StreamChunk, _>> = match &block {
                    starriver_kernel::llm::ContentBlock::ToolCall { id, .. } => vec![
                        Ok(StreamChunk::ToolCallDelta {
                            index,
                            id: id.clone(),
                            name: Some("reload_image".into()),
                            arguments_delta: r#"{"file_id":"sha256:gone"}"#.into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index,
                            block: block.clone(),
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::ToolCalls,
                            replay_state: None,
                        }),
                    ],
                    _ => vec![
                        Ok(StreamChunk::TextDelta {
                            index,
                            text: "done".into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index,
                            block: block.clone(),
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::Stop,
                            replay_state: None,
                        }),
                    ],
                };
                Box::pin(stream::iter(chunks))
            }
        }

        let tools_runtime =
            starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _handle = tools_runtime.register(definition).unwrap();
        let tools = Arc::new(tools_runtime) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(FailingAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        agent.start();
        agent.followup(message("failing reload"));
        let idle =
            tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok(), "turn must converge");

        assert_eq!(
            executions.load(Ordering::SeqCst),
            1,
            "the repeat must be short-circuited"
        );
        let events = agent.session().events();
        // The first attempt settled as a NOT_FOUND error result…
        let errors = events
            .iter()
            .filter(|e| e.event_type == "tool/result" && e.data.get("error").is_some())
            .count();
        assert_eq!(errors, 1, "exactly one failing attempt");
        // …and the repeat landed as the failure-aware stop hint (the error
        // text itself does not carry the file id, so match on the hint text).
        let gone_results: Vec<_> = events
            .iter()
            .filter(|e| {
                e.event_type == "tool/result"
                    && format!("{:?}", e.data.get("message").unwrap()).contains("sha256:gone")
            })
            .collect();
        assert_eq!(gone_results.len(), 1, "one failure hint");
        let rendered_hint = format!("{:?}", gone_results[0].data.get("message").unwrap());
        assert!(rendered_hint.contains("未成功"), "{rendered_hint}");
        assert!(rendered_hint.contains("请勿用同一 file_id 重试"), "{rendered_hint}");
        assert!(!rendered_hint.contains("已重新加载过"), "{rendered_hint}");
        assert!(
            gone_results[0].data.get("error").is_none(),
            "the stop hint is not an error"
        );
    }

    /// WI-160: turn-scoped reset — the same file id reloads again in the
    /// NEXT turn (attempted set clears at turn end).
    #[tokio::test]
    async fn reload_attempt_scope_resets_across_turns() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};
        use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct OkReload {
            executions: Arc<AtomicUsize>,
        }
        impl ToolExecutor for OkReload {
            fn execute(
                &self,
                _arguments: cordis_rslite::Value,
                _ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                let executions = Arc::clone(&self.executions);
                Box::pin(async move {
                    executions.fetch_add(1, Ordering::SeqCst);
                    Ok(cordis_rslite::Value::new(
                        serde_json::json!({ "output": "re-attached" }),
                    ))
                })
            }
        }
        let executions = Arc::new(AtomicUsize::new(0));
        let definition = ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: "reload_image".into(),
            description: "stub reload".into(),
            parameters: cordis_rslite::Value::new(()),
            output: cordis_rslite::Value::new(()),
            render: Arc::new(|_, _| Vec::new()),
            executor: Arc::new(OkReload {
                executions: Arc::clone(&executions),
            }),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        };

        /// One reload per turn: turn 1 fires on its first step; turn 2 keys
        /// on the "turn two" user message and fires before any result
        /// follows it (a plain settled count cannot see turn boundaries).
        struct TwoTurnAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for TwoTurnAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                let is_result = |b: &starriver_kernel::llm::ContentBlock| {
                    matches!(b, starriver_kernel::llm::ContentBlock::ToolResult { .. })
                };
                let turn2_start = options.messages.iter().position(|m| {
                    m.content
                        .iter()
                        .any(|b| matches!(b, starriver_kernel::llm::ContentBlock::Text(t) if t.contains("turn two")))
                });
                let wants_call = match turn2_start {
                    Some(at) => !options.messages[at..]
                        .iter()
                        .flat_map(|m| m.content.iter())
                        .any(|b| is_result(b) || matches!(b, starriver_kernel::llm::ContentBlock::ToolCall { .. })),
                    None => !options
                        .messages
                        .iter()
                        .flat_map(|m| m.content.iter())
                        .any(is_result),
                };
                let index = 0u64;
                let chunks: Vec<Result<StreamChunk, _>> = if wants_call {
                    vec![
                        Ok(StreamChunk::ToolCallDelta {
                            index,
                            id: CallId(format!("t{}", turn2_start.is_some())),
                            name: Some("reload_image".into()),
                            arguments_delta: r#"{"file_id":"sha256:again"}"#.into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index,
                            block: starriver_kernel::llm::ContentBlock::ToolCall {
                                id: CallId(format!("t{}", turn2_start.is_some())),
                                name: Some("reload_image".into()),
                                arguments: JsonValue::from_json_str(
                                    r#"{"file_id":"sha256:again"}"#,
                                )
                                .unwrap(),
                            },
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::ToolCalls,
                            replay_state: None,
                        }),
                    ]
                } else {
                    vec![
                        Ok(StreamChunk::TextDelta {
                            index,
                            text: "done".into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index,
                            block: starriver_kernel::llm::ContentBlock::Text("done".into()),
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::Stop,
                            replay_state: None,
                        }),
                    ]
                };
                Box::pin(stream::iter(chunks))
            }
        }

        let tools_runtime =
            starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _handle = tools_runtime.register(definition).unwrap();
        let tools = Arc::new(tools_runtime) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(TwoTurnAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        agent.start();
        agent.followup(message("turn one"));
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            agent.when_idle(),
        )
        .await;
        agent.followup(message("turn two"));
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            agent.when_idle(),
        )
        .await;

        // Turn 1 executed the reload; turn 2's same-id call executed again
        // (turn-scoped), then the adapter stopped.
        assert_eq!(
            executions.load(Ordering::SeqCst),
            2,
            "same id must re-execute in the next turn"
        );
    }

    /// BUG-135: a hint-immune model re-emits the same reload call every
    /// step forever. The breaker must terminate the turn: one execution,
    /// two noop stop hints, then a RELOAD_STORM error pair and turn/end —
    /// never an unbounded noop loop.
    #[tokio::test]
    async fn reload_storm_breaker_ends_the_turn() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};
        use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Counting {
            executions: Arc<AtomicUsize>,
        }
        impl ToolExecutor for Counting {
            fn execute(
                &self,
                _arguments: cordis_rslite::Value,
                _ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                let executions = Arc::clone(&self.executions);
                Box::pin(async move {
                    executions.fetch_add(1, Ordering::SeqCst);
                    Ok(cordis_rslite::Value::new(
                        serde_json::json!({ "output": "re-attached" }),
                    ))
                })
            }
        }
        let executions = Arc::new(AtomicUsize::new(0));
        let reload_definition = ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: "reload_image".into(),
            description: "stub reload".into(),
            parameters: cordis_rslite::Value::new(()),
            output: cordis_rslite::Value::new(()),
            render: Arc::new(|_, _| Vec::new()),
            executor: Arc::new(Counting {
                executions: Arc::clone(&executions),
            }),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        };

        /// Emits the SAME reload call on EVERY step — the observed storm
        /// shape (the model ignores every hint).
        struct ForeverReloadAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for ForeverReloadAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                let emitted = options
                    .messages
                    .iter()
                    .flat_map(|m| m.content.iter())
                    .filter(|b| {
                        matches!(
                            b,
                            starriver_kernel::llm::ContentBlock::ToolCall { name: Some(name), .. }
                                if name == "reload_image"
                        )
                    })
                    .count();
                let id = CallId(format!("storm-{emitted}"));
                let arguments = r#"{"file_id":"sha256:loop"}"#.to_string();
                Box::pin(stream::iter(vec![
                    Ok(StreamChunk::ToolCallDelta {
                        index: 0,
                        id,
                        name: Some("reload_image".into()),
                        arguments_delta: arguments.clone(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::ToolCall {
                            id: CallId(format!("storm-{emitted}")),
                            name: Some("reload_image".into()),
                            arguments: JsonValue::from_json_str(&arguments).unwrap(),
                        },
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::ToolCalls,
                        replay_state: None,
                    }),
                ]))
            }
        }

        let tools_runtime =
            starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _handle = tools_runtime.register(reload_definition).unwrap();
        let tools = Arc::new(tools_runtime) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(ForeverReloadAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        agent.start();
        agent.followup(message("storm"));
        let idle =
            tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok(), "the breaker must end the turn promptly");

        assert_eq!(
            executions.load(Ordering::SeqCst),
            1,
            "only the first call executes"
        );
        let events = agent.session().events();
        let calls = events
            .iter()
            .filter(|e| {
                e.event_type == "tool/call" && e.data.as_str_at("name") == Some("reload_image")
            })
            .count();
        assert_eq!(calls, 4, "execute + 2 noops + 1 breaker pair, no more");
        let storms = events
            .iter()
            .filter(|e| {
                e.event_type == "tool/result"
                    && e.data
                        .get("error")
                        .and_then(|x| x.as_str_at("code"))
                        .is_some_and(|c| c == "RELOAD_STORM")
            })
            .count();
        assert_eq!(storms, 1, "exactly one breaker result");
        assert!(
            events.iter().any(|e| e.event_type == "turn/end"),
            "the turn terminates"
        );
    }

    /// BUG-138: a tool that always fails on identical arguments — the model
    /// re-emits the same call every step (the observed echo-storm shape).
    /// Two real attempts run, then the 3rd identical call is breaker-paired
    /// without dispatch and the turn ends.
    #[tokio::test]
    async fn repeat_failure_breaker_ends_stubborn_tool_loops() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};
        use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct AlwaysFails {
            executions: Arc<AtomicUsize>,
        }
        impl ToolExecutor for AlwaysFails {
            fn execute(
                &self,
                _arguments: cordis_rslite::Value,
                _ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                let executions = Arc::clone(&self.executions);
                Box::pin(async move {
                    executions.fetch_add(1, Ordering::SeqCst);
                    Err(ToolError::NotFound("probe".into()))
                })
            }
        }
        let executions = Arc::new(AtomicUsize::new(0));
        let probe_definition = ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: "probe".into(),
            description: "always fails".into(),
            parameters: cordis_rslite::Value::new(()),
            output: cordis_rslite::Value::new(()),
            render: Arc::new(|_, _| Vec::new()),
            executor: Arc::new(AlwaysFails {
                executions: Arc::clone(&executions),
            }),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        };

        /// Emits the SAME failing probe call on EVERY step.
        struct ForeverProbeAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for ForeverProbeAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                let emitted = options
                    .messages
                    .iter()
                    .flat_map(|m| m.content.iter())
                    .filter(|b| {
                        matches!(
                            b,
                            starriver_kernel::llm::ContentBlock::ToolCall { name: Some(name), .. }
                                if name == "probe"
                        )
                    })
                    .count();
                let arguments = r#"{"path":"/x"}"#.to_string();
                Box::pin(stream::iter(vec![
                    Ok(StreamChunk::ToolCallDelta {
                        index: 0,
                        id: CallId(format!("probe-{emitted}")),
                        name: Some("probe".into()),
                        arguments_delta: arguments.clone(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::ToolCall {
                            id: CallId(format!("probe-{emitted}")),
                            name: Some("probe".into()),
                            arguments: JsonValue::from_json_str(&arguments).unwrap(),
                        },
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::ToolCalls,
                        replay_state: None,
                    }),
                ]))
            }
        }

        let tools_runtime =
            starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _handle = tools_runtime.register(probe_definition).unwrap();
        let tools = Arc::new(tools_runtime) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(ForeverProbeAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        agent.start();
        agent.followup(message("storm"));
        let idle =
            tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok(), "the breaker must end the turn promptly");

        assert_eq!(
            executions.load(Ordering::SeqCst),
            2,
            "two real attempts; the 3rd identical call is breaker-paired"
        );
        let events = agent.session().events();
        assert!(
            events.iter().any(|e| {
                e.event_type == "tool/result"
                    && e.data
                        .get("error")
                        .and_then(|x| x.as_str_at("code"))
                        .is_some_and(|c| c == "REPEAT_STORM")
            }),
            "exactly the general breaker's terminal result"
        );
        assert!(events.iter().any(|e| e.event_type == "turn/end"));
    }

    /// BUG-138 inverse: a first failure followed by a success resets the
    /// consecutive-error counter — legitimate re-runs after fixing inputs
    /// never trip the breaker.
    #[tokio::test]
    async fn legit_reruns_after_a_failure_do_not_trip_the_breaker() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};
        use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct FlakyThenOk {
            executions: Arc<AtomicUsize>,
            failed_once: Arc<AtomicBool>,
        }
        impl ToolExecutor for FlakyThenOk {
            fn execute(
                &self,
                _arguments: cordis_rslite::Value,
                _ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                let executions = Arc::clone(&self.executions);
                let failed_once = Arc::clone(&self.failed_once);
                Box::pin(async move {
                    executions.fetch_add(1, Ordering::SeqCst);
                    if !failed_once.swap(true, Ordering::SeqCst) {
                        Err(ToolError::NotFound("probe".into()))
                    } else {
                        Ok(cordis_rslite::Value::new(
                            serde_json::json!({ "output": "ok" }),
                        ))
                    }
                })
            }
        }
        let executions = Arc::new(AtomicUsize::new(0));
        let probe_definition = ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: "probe".into(),
            description: "flaky".into(),
            parameters: cordis_rslite::Value::new(()),
            output: cordis_rslite::Value::new(()),
            render: Arc::new(|_, _| Vec::new()),
            executor: Arc::new(FlakyThenOk {
                executions: Arc::clone(&executions),
                failed_once: Arc::new(AtomicBool::new(false)),
            }),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        };

        /// Emits the probe call on the first TWO steps, then finishes clean.
        struct TwiceProbeAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for TwiceProbeAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                let emitted = options
                    .messages
                    .iter()
                    .flat_map(|m| m.content.iter())
                    .filter(|b| {
                        matches!(
                            b,
                            starriver_kernel::llm::ContentBlock::ToolCall { name: Some(name), .. }
                                if name == "probe"
                        )
                    })
                    .count();
                if emitted >= 2 {
                    return Box::pin(stream::iter(vec![Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    })]));
                }
                let arguments = r#"{"path":"/x"}"#.to_string();
                Box::pin(stream::iter(vec![
                    Ok(StreamChunk::ToolCallDelta {
                        index: 0,
                        id: CallId(format!("probe-{emitted}")),
                        name: Some("probe".into()),
                        arguments_delta: arguments.clone(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::ToolCall {
                            id: CallId(format!("probe-{emitted}")),
                            name: Some("probe".into()),
                            arguments: JsonValue::from_json_str(&arguments).unwrap(),
                        },
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::ToolCalls,
                        replay_state: None,
                    }),
                ]))
            }
        }

        let tools_runtime =
            starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _handle = tools_runtime.register(probe_definition).unwrap();
        let tools = Arc::new(tools_runtime) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(TwiceProbeAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        agent.start();
        agent.followup(message("flaky"));
        let idle =
            tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok(), "the turn completes without the breaker");

        assert_eq!(executions.load(Ordering::SeqCst), 2);
        let events = agent.session().events();
        assert!(
            !events.iter().any(|e| {
                e.event_type == "tool/result"
                    && e.data
                        .get("error")
                        .and_then(|x| x.as_str_at("code"))
                        .is_some_and(|c| c == "REPEAT_STORM")
            }),
            "a failure-then-success rerun must not trip the breaker"
        );
    }

    /// BUG-135 (budget shape): nine distinct reload ids in one step — the
    /// first BUDGET (8) execute, the 9th trips the breaker, and the trailing
    /// non-reload call is paired as skipped so nothing dangles.
    #[tokio::test]
    async fn reload_turn_budget_breaks_fabricated_id_storms() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};
        use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Counting {
            executions: Arc<AtomicUsize>,
        }
        impl ToolExecutor for Counting {
            fn execute(
                &self,
                _arguments: cordis_rslite::Value,
                _ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                let executions = Arc::clone(&self.executions);
                Box::pin(async move {
                    executions.fetch_add(1, Ordering::SeqCst);
                    Ok(cordis_rslite::Value::new(
                        serde_json::json!({ "output": "re-attached" }),
                    ))
                })
            }
        }
        let reload_executions = Arc::new(AtomicUsize::new(0));
        let echo_executions = Arc::new(AtomicUsize::new(0));
        let counting = |name: &'static str, executions: Arc<AtomicUsize>| ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: name.into(),
            description: format!("{name} stub").into(),
            parameters: cordis_rslite::Value::new(()),
            output: cordis_rslite::Value::new(()),
            render: Arc::new(|_, _| Vec::new()),
            executor: Arc::new(Counting { executions }),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        };
        let reload_definition = counting("reload_image", Arc::clone(&reload_executions));
        let echo_definition = counting("echo", Arc::clone(&echo_executions));

        /// Step 1: nine distinct reload ids then an echo; step 2: done.
        struct NineIdsAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for NineIdsAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                let settled = options
                    .messages
                    .iter()
                    .flat_map(|m| m.content.iter())
                    .filter(|b| {
                        matches!(b, starriver_kernel::llm::ContentBlock::ToolResult { .. })
                    })
                    .count();
                let mut calls: Vec<(String, String)> = Vec::new();
                if settled == 0 {
                    for i in 0..9 {
                        calls.push((
                            format!("fab-{i}"),
                            format!(r#"{{"file_id":"sha256:fabricated{i}"}}"#),
                        ));
                    }
                    calls.push(("tail-echo".into(), "{}".into()));
                }
                let mut chunks: Vec<Result<StreamChunk, _>> = Vec::new();
                for (index, (id, args)) in calls.iter().enumerate() {
                    let index = index as u64;
                    let name = if id.starts_with("fab") {
                        "reload_image"
                    } else {
                        "echo"
                    };
                    chunks.push(Ok(StreamChunk::ToolCallDelta {
                        index,
                        id: CallId(id.clone()),
                        name: Some(name.into()),
                        arguments_delta: args.clone(),
                    }));
                    chunks.push(Ok(StreamChunk::BlockEnd {
                        index,
                        block: starriver_kernel::llm::ContentBlock::ToolCall {
                            id: CallId(id.clone()),
                            name: Some(name.into()),
                            arguments: JsonValue::from_json_str(args).unwrap(),
                        },
                    }));
                }
                let finish = if settled == 0 {
                    FinishReason::ToolCalls
                } else {
                    chunks.push(Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "done".into(),
                    }));
                    chunks.push(Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::Text("done".into()),
                    }));
                    FinishReason::Stop
                };
                chunks.push(Ok(StreamChunk::Finish {
                    reason: finish,
                    replay_state: None,
                }));
                Box::pin(stream::iter(chunks))
            }
        }

        let tools_runtime =
            starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _h1 = tools_runtime.register(reload_definition).unwrap();
        let _h2 = tools_runtime.register(echo_definition).unwrap();
        let tools = Arc::new(tools_runtime) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(NineIdsAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        agent.start();
        agent.followup(message("fabricated storm"));
        let idle =
            tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok(), "turn must converge");

        assert_eq!(
            reload_executions.load(Ordering::SeqCst),
            8,
            "the first BUDGET calls execute; the 9th breaks"
        );
        assert_eq!(
            echo_executions.load(Ordering::SeqCst),
            0,
            "the trailing call is paired as skipped, never runs"
        );
        let events = agent.session().events();
        let storms = events
            .iter()
            .filter(|e| {
                e.event_type == "tool/result"
                    && e.data
                        .get("error")
                        .and_then(|x| x.as_str_at("code"))
                        .is_some_and(|c| c == "RELOAD_STORM")
            })
            .count();
        assert_eq!(storms, 1);
        let skipped = events
            .iter()
            .filter(|e| {
                e.event_type == "tool/result"
                    && e.data
                        .get("error")
                        .and_then(|x| x.as_str_at("code"))
                        .is_some_and(|c| c == "ABORTED_BEFORE_DISPATCH")
            })
            .count();
        assert_eq!(skipped, 1, "the trailing echo is paired, not dangling");
        assert!(events.iter().any(|e| e.event_type == "turn/end"));
    }

    /// WI-160: two DIFFERENT ids in one step must both execute (no false
    /// duplicate), and an unparseable-arguments reload call flows to the
    /// executor (INVALID args) instead of the noop path.
    #[tokio::test]
    async fn distinct_ids_execute_and_bad_args_reach_the_executor() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};
        use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingExec {
            executions: Arc<AtomicUsize>,
        }
        impl ToolExecutor for CountingExec {
            fn execute(
                &self,
                _arguments: cordis_rslite::Value,
                _ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                let executions = Arc::clone(&self.executions);
                Box::pin(async move {
                    executions.fetch_add(1, Ordering::SeqCst);
                    Ok(cordis_rslite::Value::new(
                        serde_json::json!({ "output": "ok" }),
                    ))
                })
            }
        }
        let executions = Arc::new(AtomicUsize::new(0));
        let definition = ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: "reload_image".into(),
            description: "stub reload".into(),
            parameters: cordis_rslite::Value::new(()),
            output: cordis_rslite::Value::new(()),
            render: Arc::new(|_, _| Vec::new()),
            executor: Arc::new(CountingExec {
                executions: Arc::clone(&executions),
            }),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        };

        /// Step 1: two reloads with DIFFERENT ids + one with missing
        /// file_id; step 2: done.
        struct MixedAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MixedAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                let settled = options
                    .messages
                    .iter()
                    .flat_map(|m| m.content.iter())
                    .filter(|b| {
                        matches!(b, starriver_kernel::llm::ContentBlock::ToolResult { .. })
                    })
                    .count();
                let calls: Vec<(String, String)> = if settled == 0 {
                    vec![
                        ("d1".into(), r#"{"file_id":"sha256:one"}"#.into()),
                        ("d2".into(), r#"{"file_id":"sha256:two"}"#.into()),
                        ("d3".into(), r#"{"other": 1}"#.into()),
                    ]
                } else {
                    Vec::new()
                };
                let mut chunks: Vec<Result<StreamChunk, _>> = Vec::new();
                for (index, (id, args)) in calls.iter().enumerate() {
                    let index = index as u64;
                    chunks.push(Ok(StreamChunk::ToolCallDelta {
                        index,
                        id: CallId(id.clone()),
                        name: Some("reload_image".into()),
                        arguments_delta: args.clone(),
                    }));
                    chunks.push(Ok(StreamChunk::BlockEnd {
                        index,
                        block: starriver_kernel::llm::ContentBlock::ToolCall {
                            id: CallId(id.clone()),
                            name: Some("reload_image".into()),
                            arguments: JsonValue::from_json_str(args).unwrap(),
                        },
                    }));
                }
                let finish = if settled == 0 {
                    FinishReason::ToolCalls
                } else {
                    chunks.push(Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "done".into(),
                    }));
                    chunks.push(Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::Text("done".into()),
                    }));
                    FinishReason::Stop
                };
                chunks.push(Ok(StreamChunk::Finish {
                    reason: finish,
                    replay_state: None,
                }));
                Box::pin(stream::iter(chunks))
            }
        }

        let tools_runtime =
            starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _handle = tools_runtime.register(definition).unwrap();
        let tools = Arc::new(tools_runtime) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MixedAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        agent.start();
        agent.followup(message("mixed"));
        let idle =
            tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok(), "turn must converge");

        // All three calls reached the executor (two distinct ids + the
        // unparseable one — nothing falsely short-circuited).
        assert_eq!(executions.load(Ordering::SeqCst), 3);
        let noop = agent
            .session()
            .events()
            .iter()
            .filter(|e| {
                e.event_type == "tool/result"
                    && format!("{:?}", e.data.get("message").unwrap()).contains("无需重复调用")
            })
            .count();
        assert_eq!(noop, 0, "no false duplicates");
    }

    #[tokio::test]
    async fn tool_body_panic_is_contained_not_driver_panic() {
        use starriver_kernel::agent::AgentErrorEvent;
        use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct PanicTool;
        impl ToolExecutor for PanicTool {
            fn execute(
                &self,
                _arguments: cordis_rslite::Value,
                _ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                Box::pin(async { panic!("tool body panic") })
            }
        }

        let tools_runtime =
            starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _h1 = tools_runtime
            .register(ToolDefinition {
                requires_approval: false,
                deny_in_readonly: false,
                name: "boom".into(),
                description: "boom".into(),
                parameters: cordis_rslite::Value::new(()),
                output: cordis_rslite::Value::new(()),
                render: Arc::new(|_, value| {
                    vec![starriver_kernel::llm::ContentBlock::Text(format!(
                        "{value:?}"
                    ))]
                }),
                executor: Arc::new(PanicTool),
                is_concurrency_safe: None,
                timeout: None,
                finalize_content: None,
            })
            .unwrap();
        let tools = Arc::new(tools_runtime) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(ToolCallAdapter {
                providers: vec!["mock".into()],
                first: std::sync::atomic::AtomicBool::new(true),
            }),
            tools,
        );
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let _listener = agent.ctx().on::<AgentErrorEvent, _>(move |_| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(cordis_rslite::Value::new(()))
            })
        });
        agent.start();
        agent.followup(message("a"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok());
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert_eq!(
            seen.load(Ordering::SeqCst),
            0,
            "a contained tool panic must not surface as a driver panic"
        );
        let events = agent.session().events();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "tool/result")
                .count(),
            1
        );
        let result = events
            .iter()
            .find(|e| e.event_type == "tool/result")
            .unwrap();
        assert_eq!(
            result.data.get("error").and_then(|e| e.as_str_at("code")),
            Some("TOOL_PANIC"),
            "tool panic normalizes to a tool error result"
        );
        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("completed"),
            "a normalized tool error result must not abort the turn"
        );
    }

    struct ToolCallAdapter {
        providers: Vec<String>,
        first: std::sync::atomic::AtomicBool,
    }
    impl starriver_kernel::llm::LlmAdapter for ToolCallAdapter {
        fn providers(&self) -> &[String] {
            &self.providers
        }
        fn stream(
            &self,
            _options: starriver_kernel::llm::GenerateOptions,
        ) -> futures::stream::BoxStream<
            'static,
            Result<starriver_kernel::llm::StreamChunk, starriver_kernel::llm::LlmError>,
        > {
            use futures::stream;
            if !self.first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                let text = "after-tool".to_string();
                return Box::pin(stream::iter(vec![
                    Ok(starriver_kernel::llm::StreamChunk::TextDelta {
                        index: 0,
                        text: text.clone(),
                    }),
                    Ok(starriver_kernel::llm::StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::Text(text),
                    }),
                    Ok(starriver_kernel::llm::StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]));
            }
            Box::pin(stream::iter(vec![
                Ok(starriver_kernel::llm::StreamChunk::ToolCallDelta {
                    index: 0,
                    id: CallId("c1".into()),
                    name: Some("boom".into()),
                    arguments_delta: "{}".into(),
                }),
                Ok(starriver_kernel::llm::StreamChunk::BlockEnd {
                    index: 0,
                    block: starriver_kernel::llm::ContentBlock::ToolCall {
                        id: CallId("c1".into()),
                        name: Some("boom".into()),
                        arguments: JsonValue::Object(Vec::new()),
                    },
                }),
                Ok(starriver_kernel::llm::StreamChunk::Finish {
                    reason: FinishReason::ToolCalls,
                    replay_state: None,
                }),
            ]))
        }
    }

    #[tokio::test]
    async fn repeated_panics_are_reported_and_agent_survives() {
        use futures::stream;
        use starriver_kernel::agent::AgentErrorEvent;
        use starriver_kernel::llm::{
            FinishReason, GenerateOptions, LlmCallConfig, LlmError, PreparedLlmCall, StreamChunk,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct PanicTwiceLlm {
            calls: AtomicUsize,
        }
        impl crate::services::LlmRuntimeView for PanicTwiceLlm {
            fn prepare_call(
                &self,
                _config: LlmCallConfig,
                _cancellation: Option<CancellationToken>,
            ) -> cordis_rslite::BoxFuture<Result<PreparedLlmCall, LlmError>> {
                Box::pin(async { Err(LlmError::NoAdapter("mock".into())) })
            }
            fn stream(
                &self,
                _options: GenerateOptions,
            ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call < 2 {
                    Box::pin(stream::once(async { panic!("repeated panic") }))
                } else {
                    Box::pin(stream::iter(vec![
                        Ok(StreamChunk::TextDelta {
                            index: 0,
                            text: "ok".into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index: 0,
                            block: starriver_kernel::llm::ContentBlock::Text("ok".into()),
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::Stop,
                            replay_state: None,
                        }),
                    ]))
                }
            }
        }

        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let llm = Arc::new(PanicTwiceLlm {
            calls: AtomicUsize::new(0),
        }) as Arc<dyn crate::services::LlmRuntimeView>;
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let session = Session::new(SessionId("a1".into()), bus);
        let agent = ReactLoopAgent::new(
            ctx,
            SessionId("a1".into()),
            starriver_kernel::agent::AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let _listener = agent.ctx().on::<AgentErrorEvent, _>(move |_| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(cordis_rslite::Value::new(()))
            })
        });
        agent.start();

        for _ in 0..2 {
            agent.followup(message("x"));
            let idle =
                tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
            assert!(idle.is_ok());
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(seen.load(Ordering::SeqCst), 2, "each panic is reported");

        agent.followup(message("y"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok());
        let events = agent.session().events();
        let last_turn_end = events
            .iter()
            .rev()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            last_turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn max_tokens_is_sticky_across_steps() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};
        use std::sync::atomic::{AtomicBool, Ordering};

        struct MockAdapter {
            providers: Vec<String>,
            first: AtomicBool,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                if self.first.swap(false, Ordering::SeqCst) {
                    Box::pin(stream::iter(vec![Ok(StreamChunk::Finish {
                        reason: FinishReason::MaxTokens,
                        replay_state: None,
                    })]))
                } else {
                    Box::pin(stream::iter(vec![
                        Ok(StreamChunk::TextDelta {
                            index: 0,
                            text: "after".into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index: 0,
                            block: starriver_kernel::llm::ContentBlock::Text("after".into()),
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::Stop,
                            replay_state: None,
                        }),
                    ]))
                }
            }
        }

        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
                first: AtomicBool::new(true),
            }),
            tools,
        );
        agent.start();
        agent.followup(message("a"));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        agent.steer(message("b"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok());

        let events = agent.session().events();
        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("max-tokens"),
            "a later completed step must not downgrade the max-tokens outcome"
        );
    }

    #[tokio::test]
    async fn turn_stopping_can_extend_the_turn() {
        use futures::stream;
        use starriver_kernel::agent::AgentTurnStopping;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct MockAdapter {
            providers: Vec<String>,
            calls: AtomicUsize,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                let text = format!("reply-{call}");
                Box::pin(stream::iter(vec![
                    Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: text.clone(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::Text(text),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]))
            }
        }

        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
                calls: AtomicUsize::new(0),
            }),
            tools,
        );
        let injected = Arc::new(AtomicBool::new(false));
        let hook_calls = Arc::new(AtomicUsize::new(0));
        let steer_target = agent.clone();
        let injected_flag = Arc::clone(&injected);
        let hook_counter = Arc::clone(&hook_calls);
        let _hook = agent.ctx().on_sync::<AgentTurnStopping, _>(move |_| {
            hook_counter.fetch_add(1, Ordering::SeqCst);
            if !injected_flag.swap(true, Ordering::SeqCst) {
                steer_target.steer(message("extra"));
            }
            Ok(cordis_rslite::Value::new(()))
        });
        agent.start();
        agent.followup(message("a"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok());

        assert!(
            hook_calls.load(Ordering::SeqCst) >= 2,
            "hook runs per close attempt"
        );
        let events = agent.session().events();
        assert!(
            events
                .iter()
                .filter(|e| e.event_type == "user/message")
                .any(|e| e.data.as_str_at("id") == Some("extra")),
            "turn-stopping injected input must be consumed"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "assistant/message")
                .count(),
            2,
            "the turn must continue after the hook injected next-step input"
        );
    }

    #[tokio::test]
    async fn agent_request_proposal_chain_preserves_listener_edits() {
        use futures::stream;
        use starriver_kernel::agent::AgentRequest;
        use starriver_kernel::llm::{CallId, FinishReason, LlmAdapter, StreamChunk};
        use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

        struct MockAdapter {
            providers: Vec<String>,
            calls: std::sync::atomic::AtomicUsize,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Box::pin(stream::iter(vec![
                        Ok(StreamChunk::ToolCallDelta {
                            index: 0,
                            id: CallId("c1".into()),
                            name: Some("echo".into()),
                            arguments_delta: "{}".into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index: 0,
                            block: starriver_kernel::llm::ContentBlock::ToolCall {
                                id: CallId("c1".into()),
                                name: Some("echo".into()),
                                arguments: JsonValue::Object(Vec::new()),
                            },
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::ToolCalls,
                            replay_state: None,
                        }),
                    ]))
                } else {
                    Box::pin(stream::iter(vec![
                        Ok(StreamChunk::TextDelta {
                            index: 0,
                            text: "done".into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index: 0,
                            block: starriver_kernel::llm::ContentBlock::Text("done".into()),
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::Stop,
                            replay_state: None,
                        }),
                    ]))
                }
            }
        }

        struct Echo;
        impl ToolExecutor for Echo {
            fn execute(
                &self,
                arguments: cordis_rslite::Value,
                _ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                Box::pin(async { Ok(arguments) })
            }
        }

        let tools_runtime =
            starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _h1 = tools_runtime
            .register(ToolDefinition {
                requires_approval: false,
                deny_in_readonly: false,
                name: "echo".into(),
                description: "echo".into(),
                parameters: cordis_rslite::Value::new(()),
                output: cordis_rslite::Value::new(()),
                render: Arc::new(|_, value| {
                    vec![starriver_kernel::llm::ContentBlock::Text(format!(
                        "{value:?}"
                    ))]
                }),
                executor: Arc::new(Echo),
                is_concurrency_safe: None,
                timeout: None,
                finalize_content: None,
            })
            .unwrap();
        let tools = Arc::new(tools_runtime) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            tools,
        );

        let seen = Arc::new(std::sync::Mutex::new(Vec::<Option<f64>>::new()));
        let recorder = Arc::clone(&seen);
        let _request = agent
            .ctx()
            .on_waterfall::<AgentRequest, _>(move |_event, next| {
                let recorder = Arc::clone(&recorder);
                Box::pin(async move {
                    let proposed = next().await.unwrap();
                    recorder.lock().unwrap().push(proposed.temperature);
                    Ok(starriver_kernel::llm::LlmCallConfig {
                        temperature: Some(0.5),
                        ..proposed
                    })
                })
            });
        agent.start();
        agent.followup(message("a"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok());

        let proposals = seen.lock().unwrap().clone();
        assert_eq!(
            proposals.len(),
            2,
            "a tool turn produces two model requests"
        );
        // Step 0's seed carries no temperature yet; step 1's seed must inherit
        // the effective config from step 0, including the listener's edit.
        assert_eq!(proposals[0], None, "initial proposal has no temperature");
        assert_eq!(
            proposals[1],
            Some(0.5),
            "step 1: the listener's temperature edit must survive the proposal chain"
        );
    }

    #[tokio::test]
    async fn duplicate_send_panics_and_agent_survives() {
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                use futures::stream;
                Box::pin(stream::iter(vec![Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                })]))
            }
        }

        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        agent.start();
        agent.followup(message("a"));
        agent.followup(message("b"));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            agent.followup(message("a"));
        }));
        assert!(result.is_err(), "duplicate pending id must panic on send");

        let events = agent.session().events();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "agent/inbox/spliced")
                .count(),
            2,
            "the failed splice must not write a durable event"
        );

        // The agent (and its inbox mutex) survive the caller-side panic.
        agent.followup(message("c"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(
            idle.is_ok(),
            "agent must keep working after a duplicate send"
        );
    }

    #[tokio::test]
    async fn max_tokens_without_pending_closes_directly() {
        use futures::stream;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                Box::pin(stream::iter(vec![Ok(StreamChunk::Finish {
                    reason: FinishReason::MaxTokens,
                    replay_state: None,
                })]))
            }
        }

        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        agent.start();
        agent.followup(message("a"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok());

        let events = agent.session().events();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "turn/start")
                .count(),
            1
        );
        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("max-tokens")
        );
    }

    #[tokio::test]
    async fn turn_stopping_without_injection_closes() {
        use starriver_kernel::agent::AgentTurnStopping;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                use futures::stream;
                Box::pin(stream::iter(vec![
                    Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "done".into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::Text("done".into()),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]))
            }
        }

        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        let hook_calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hook_calls);
        let _hook = agent.ctx().on_sync::<AgentTurnStopping, _>(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(cordis_rslite::Value::new(()))
        });
        agent.start();
        agent.followup(message("a"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok());

        assert_eq!(
            hook_calls.load(Ordering::SeqCst),
            1,
            "hook runs once at close"
        );
        let events = agent.session().events();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event_type == "assistant/message")
                .count(),
            1
        );
        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("completed")
        );
    }

    #[tokio::test]
    async fn turn_stopping_cancel_aborts_turn() {
        use starriver_kernel::agent::AgentTurnStopping;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                use futures::stream;
                Box::pin(stream::iter(vec![
                    Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "done".into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::Text("done".into()),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]))
            }
        }

        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        let cancel_target = agent.clone();
        let _hook = agent.ctx().on_sync::<AgentTurnStopping, _>(move |_| {
            cancel_target.cancel(
                starriver_kernel::agent::AgentCancelCause::new(
                    starriver_kernel::agent::AgentCancelKind::User,
                ),
                false,
            );
            Ok(cordis_rslite::Value::new(()))
        });
        agent.start();
        agent.followup(message("a"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok());

        let events = agent.session().events();
        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("kind")),
            Some("aborted"),
            "cancelling inside turn-stopping must abort the turn"
        );
    }

    #[tokio::test]
    async fn adapter_defaulted_max_tokens_is_removed_from_next_seed() {
        use futures::stream;
        use starriver_kernel::agent::AgentRequest;
        use starriver_kernel::llm::{
            CallId, FinishReason, LlmAdapter, LlmResolvedModelInfo, StreamChunk,
        };
        use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct MockAdapter {
            providers: Vec<String>,
            calls: AtomicUsize,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn resolve_model(
                &self,
                provider: &str,
                model: &str,
                _cancellation: Option<CancellationToken>,
            ) -> cordis_rslite::BoxFuture<Result<LlmResolvedModelInfo, LlmError>> {
                let provider = provider.to_string();
                let model = model.to_string();
                Box::pin(async move {
                    Ok(LlmResolvedModelInfo {
                        provider,
                        id: model.clone(),
                        name: model,
                        description: None,
                        input_modalities: None,
                        context: None,
                        default_max_tokens: Some(100),
                        reasoning: None,
                    })
                })
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Box::pin(stream::iter(vec![
                        Ok(StreamChunk::ToolCallDelta {
                            index: 0,
                            id: CallId("c1".into()),
                            name: Some("echo".into()),
                            arguments_delta: "{}".into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index: 0,
                            block: starriver_kernel::llm::ContentBlock::ToolCall {
                                id: CallId("c1".into()),
                                name: Some("echo".into()),
                                arguments: JsonValue::Object(Vec::new()),
                            },
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::ToolCalls,
                            replay_state: None,
                        }),
                    ]))
                } else {
                    Box::pin(stream::iter(vec![
                        Ok(StreamChunk::TextDelta {
                            index: 0,
                            text: "done".into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index: 0,
                            block: starriver_kernel::llm::ContentBlock::Text("done".into()),
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::Stop,
                            replay_state: None,
                        }),
                    ]))
                }
            }
        }

        struct Echo;
        impl ToolExecutor for Echo {
            fn execute(
                &self,
                arguments: cordis_rslite::Value,
                _ctx: ToolRunContext,
            ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, ToolError>> {
                Box::pin(async { Ok(arguments) })
            }
        }

        let tools_runtime =
            starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
        let _h1 = tools_runtime
            .register(ToolDefinition {
                requires_approval: false,
                deny_in_readonly: false,
                name: "echo".into(),
                description: "echo".into(),
                parameters: cordis_rslite::Value::new(()),
                output: cordis_rslite::Value::new(()),
                render: Arc::new(|_, value| {
                    vec![starriver_kernel::llm::ContentBlock::Text(format!(
                        "{value:?}"
                    ))]
                }),
                executor: Arc::new(Echo),
                is_concurrency_safe: None,
                timeout: None,
                finalize_content: None,
            })
            .unwrap();
        let tools = Arc::new(tools_runtime) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
                calls: AtomicUsize::new(0),
            }),
            tools,
        );

        let seen = Arc::new(std::sync::Mutex::new(Vec::<Option<u64>>::new()));
        let recorder = Arc::clone(&seen);
        let _request = agent
            .ctx()
            .on_waterfall::<AgentRequest, _>(move |_event, next| {
                let recorder = Arc::clone(&recorder);
                Box::pin(async move {
                    let proposed = next().await.unwrap();
                    recorder.lock().unwrap().push(proposed.max_tokens);
                    Ok(proposed)
                })
            });
        agent.start();
        agent.followup(message("a"));
        let idle = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle.is_ok());

        let proposals = seen.lock().unwrap().clone();
        assert_eq!(proposals.len(), 2);
        // Step 0: nothing to inherit; step 1: the adapter-defaulted max_tokens
        // is removed from the seed so the adapter re-materializes it.
        assert_eq!(proposals[0], None);
        assert_eq!(
            proposals[1], None,
            "adapter-defaulted max_tokens must not leak into the next proposal"
        );

        // The durable request header records which fields came from the adapter.
        let events = agent.session().events();
        let header = events
            .iter()
            .find(|e| e.event_type == "request/header")
            .expect("request/header");
        assert_eq!(
            header
                .data
                .get("adapterDefaults")
                .and_then(|d| d.get("maxTokens"))
                .and_then(JsonValue::as_bool),
            Some(true)
        );
    }

    #[tokio::test]
    async fn pending_messages_reflects_inbox() {
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                use futures::stream;
                Box::pin(stream::iter(vec![Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                })]))
            }
        }

        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        agent.start();
        agent.followup(message("a"));
        agent.inject(message("b"));

        let pending = agent.pending_messages();
        assert_eq!(pending.next_turn.len(), 1);
        assert_eq!(pending.next_turn[0].id.0, "a");
        assert_eq!(pending.next_step.len(), 1);
        assert_eq!(pending.next_step[0].id.0, "b");
    }

    #[tokio::test]
    async fn agent_status_events_are_emitted() {
        use starriver_kernel::agent::AgentStatusChanged;
        use starriver_kernel::llm::{FinishReason, LlmAdapter, StreamChunk};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                _options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                use futures::stream;
                Box::pin(stream::iter(vec![
                    Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "done".into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::Text("done".into()),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]))
            }
        }

        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        let running = Arc::new(AtomicUsize::new(0));
        let idle = Arc::new(AtomicUsize::new(0));
        let running_counter = Arc::clone(&running);
        let idle_counter = Arc::clone(&idle);
        let _listener = agent.ctx().on_sync::<AgentStatusChanged, _>(move |event| {
            match event.status {
                AgentStatus::Running => {
                    running_counter.fetch_add(1, Ordering::SeqCst);
                }
                AgentStatus::Idle => {
                    idle_counter.fetch_add(1, Ordering::SeqCst);
                }
            }
            Ok(cordis_rslite::Value::new(()))
        });
        agent.start();
        agent.followup(message("a"));
        let idle_ok =
            tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle_ok.is_ok());
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert!(
            running.load(Ordering::SeqCst) >= 1,
            "Running must be emitted"
        );
        assert!(idle.load(Ordering::SeqCst) >= 1, "Idle must be emitted");
    }

    #[tokio::test]
    async fn idle_cancel_does_not_leak_cause() {
        use futures::stream;
        use starriver_kernel::llm::{LlmAdapter, LlmError, StreamChunk};

        struct MockAdapter {
            providers: Vec<String>,
        }
        impl LlmAdapter for MockAdapter {
            fn providers(&self) -> &[String] {
                &self.providers
            }
            fn stream(
                &self,
                options: starriver_kernel::llm::GenerateOptions,
            ) -> futures::stream::BoxStream<
                'static,
                Result<StreamChunk, starriver_kernel::llm::LlmError>,
            > {
                let cancellation = options.cancellation.clone();
                Box::pin(
                    stream::iter(vec![Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "partial".into(),
                    })])
                    .chain(stream::once(async move {
                        if let Some(token) = cancellation {
                            token.cancelled().await;
                        }
                        Err(LlmError::Cancelled)
                    })),
                )
            }
        }

        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn crate::services::ToolRuntimeView>;
        let agent = agent_with(
            Arc::new(MockAdapter {
                providers: vec!["mock".into()],
            }),
            tools,
        );
        agent.start();
        // Idle no-op cancel must not record a cause.
        agent.cancel(
            starriver_kernel::agent::AgentCancelCause::new(
                starriver_kernel::agent::AgentCancelKind::User,
            ),
            false,
        );
        agent.followup(message("a"));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        agent.cancel(
            starriver_kernel::agent::AgentCancelCause::new(
                starriver_kernel::agent::AgentCancelKind::Parent,
            ),
            false,
        );
        let idle_ok =
            tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
        assert!(idle_ok.is_ok());

        let events = agent.session().events();
        let turn_end = events
            .iter()
            .find(|e| e.event_type == "turn/end")
            .expect("turn/end");
        assert_eq!(
            turn_end
                .data
                .get("reason")
                .and_then(|r| r.as_str_at("cause")),
            Some("parent"),
            "the real abort cause must win over any idle no-op"
        );
    }

    #[test]
    fn assemble_system_merges_sections_sorted() {
        let options = AgentOptions {
            compaction_tuning: None,
            system: Some("legacy".into()),
            system_sections: vec![("z".into(), "zeta".into()), ("a".into(), "alpha".into())],
            ..Default::default()
        };
        let assembled = super::assemble_system(&options).unwrap();
        assert!(assembled.starts_with("alpha"));
        assert!(assembled.ends_with("zeta"));
        assert!(!assembled.contains("legacy"));
    }

    #[test]
    fn assemble_system_falls_back_to_legacy_system() {
        let options = AgentOptions {
            compaction_tuning: None,
            system: Some("legacy".into()),
            system_sections: Vec::new(),
            ..Default::default()
        };
        assert_eq!(super::assemble_system(&options).as_deref(), Some("legacy"));
    }

    #[test]
    fn assemble_system_empty_everything_is_none() {
        let options = AgentOptions::default();
        assert_eq!(super::assemble_system(&options), None);
    }

    #[test]
    fn assemble_system_single_section_keeps_text() {
        let options = AgentOptions {
            compaction_tuning: None,
            system_sections: vec![("role".into(), "be helpful".into())],
            ..Default::default()
        };
        assert_eq!(
            super::assemble_system(&options).as_deref(),
            Some("be helpful")
        );
    }

    #[test]
    fn assemble_system_skips_empty_section_text() {
        // Empty section text contributes nothing but still participates in
        // ordering; joining non-empty texts yields the surviving ones.
        let options = AgentOptions {
            compaction_tuning: None,
            system_sections: vec![
                ("a".into(), "alpha".into()),
                ("b".into(), String::new()),
                ("c".into(), "gamma".into()),
            ],
            ..Default::default()
        };
        let assembled = super::assemble_system(&options).unwrap();
        assert!(assembled.contains("alpha"));
        assert!(assembled.contains("gamma"));
        // Empty section text contributes an empty segment between newlines;
        // the surviving texts are still present and ordered.
        assert!(assembled.starts_with("alpha"));
        assert!(assembled.ends_with("gamma"));
    }

    #[test]
    fn render_snapshot_joins_and_prefixes() {
        let sections = vec![
            starriver_kernel::context::AssembledContext {
                name: "cwd".into(),
                text: "/tmp".into(),
            },
            starriver_kernel::context::AssembledContext {
                name: "time".into(),
                text: "now".into(),
            },
        ];
        let snapshot = super::render_snapshot(&sections);
        assert!(snapshot.starts_with("Current runtime context"));
        assert!(snapshot.contains("/tmp"));
        assert!(snapshot.contains("now"));
    }

    #[test]
    fn render_snapshot_empty_when_no_sections() {
        assert_eq!(super::render_snapshot(&[]), "");
    }

    // ── Compaction (SP-c): threshold → summarize → replace checkpoint ──────

    /// Adapter that records incoming requests and answers every call with one
    /// text block carrying `summary_text` (or a failure when `fail` is set).
    struct CompactionStubAdapter {
        providers: Vec<String>,
        requests: Arc<Mutex<Vec<starriver_kernel::llm::GenerateOptions>>>,
        summary_text: String,
        fail: bool,
    }

    impl starriver_kernel::llm::LlmAdapter for CompactionStubAdapter {
        fn providers(&self) -> &[String] {
            &self.providers
        }
        fn stream(
            &self,
            options: starriver_kernel::llm::GenerateOptions,
        ) -> futures::stream::BoxStream<
            'static,
            Result<starriver_kernel::llm::StreamChunk, starriver_kernel::llm::LlmError>,
        > {
            self.requests.lock().unwrap().push(options);
            let out: Vec<
                Result<starriver_kernel::llm::StreamChunk, starriver_kernel::llm::LlmError>,
            > = if self.fail {
                vec![Err(starriver_kernel::llm::LlmError::Call {
                    code: "PROVIDER_DOWN".into(),
                    message: "boom".into(),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                })]
            } else {
                vec![
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::Text(self.summary_text.clone()),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]
            };
            Box::pin(futures::stream::iter(out))
        }
    }

    fn compaction_agent(
        summary_text: &str,
        fail: bool,
        requests: Arc<Mutex<Vec<starriver_kernel::llm::GenerateOptions>>>,
    ) -> Arc<ReactLoopAgent> {
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let runtime = starriver_llm::InMemoryLlmRuntime::new(EventBus::new());
        let _handle = runtime
            .register_adapter(Arc::new(CompactionStubAdapter {
                providers: vec!["mock".into()],
                requests,
                summary_text: summary_text.into(),
                fail,
            }))
            .unwrap();
        let llm = Arc::new(runtime) as Arc<dyn LlmRuntimeView>;
        let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
            EventBus::new(),
            Context::new(),
        )) as Arc<dyn ToolRuntimeView>;
        let session = Session::new(SessionId("c1".into()), bus);
        ReactLoopAgent::new(
            ctx,
            SessionId("c1".into()),
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                ..Default::default()
            },
            session,
            llm,
            tools,
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        )
    }

    fn append_surface(agent: &ReactLoopAgent, event_type: &str, message: Message) {
        agent
            .session
            .append_with(
                event_type,
                JsonValue::Object(vec![
                    ("turn".into(), JsonValue::UInt(1)),
                    ("step".into(), JsonValue::UInt(1)),
                    ("message".into(), message.to_json().expect("message json")),
                ]),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: Vec::new(),
                },
            )
            .expect("surface append");
    }

    fn seed_user_message(agent: &ReactLoopAgent, text: &str) {
        // user/message data is the bare message JSON (append_snapshot_message
        // parity); the wrapped {turn, step, message} shape is for assistant
        // and tool-result events.
        agent
            .session
            .append_with(
                "user/message",
                Message {
                    id: MessageId(format!("u-{}", agent.session.seq())),
                    role: MessageRole::User,
                    content: vec![ContentBlock::Text(text.into())],
                    source: MessageSource::User,
                }
                .to_json()
                .expect("message json"),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: Vec::new(),
                },
            )
            .expect("surface append");
    }

    fn seed_tool_pair(agent: &ReactLoopAgent, call_id: &str) {
        append_surface(
            agent,
            "assistant/message",
            Message {
                id: MessageId(call_id.into()),
                role: MessageRole::Assistant,
                content: vec![ContentBlock::ToolCall {
                    id: CallId(call_id.into()),
                    name: Some("bash".into()),
                    arguments: starriver_kernel::json::JsonValue::String("x".repeat(400)),
                }],
                source: MessageSource::Model {
                    provider: "mock".into(),
                    model: "m1".into(),
                    replay_state: None,
                },
            },
        );
        append_surface(
            agent,
            "tool/result",
            Message {
                id: MessageId(format!("{call_id}-result")),
                role: MessageRole::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: CallId(call_id.into()),
                    content: vec![ContentBlock::Text("x".repeat(400))],
                    is_error: false,
                }],
                source: MessageSource::Tool {
                    call_id: CallId(call_id.into()),
                },
            },
        );
    }

    fn compaction_events(agent: &ReactLoopAgent, kind: &str) -> Vec<JsonValue> {
        agent
            .session
            .events()
            .into_iter()
            .filter(|event| event.event_type == kind)
            .map(|event| event.data)
            .collect()
    }

    #[tokio::test]
    async fn compaction_replaces_summarized_prefix_and_keeps_tail_verbatim() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let agent = compaction_agent(
            "## Primary Request and Intent\nSUMMARY-TEXT",
            false,
            Arc::clone(&requests),
        );
        // 8 user messages × 250 estimated tokens = 2000 > 0.8 × 1000.
        for _ in 0..8 {
            seed_user_message(&agent, &"x".repeat(1000));
        }

        agent.compact_if_needed(Some("SYS"), 1000, 0.8).await;

        let derived = agent.session.derive_messages_with_seqs().unwrap();
        assert_eq!(derived.len(), 3, "tail (2) + checkpoint expected");
        assert!(
            matches!(&derived[0].1.content[0], ContentBlock::Text(t) if *t == "x".repeat(1000)),
            "retained tail must stay verbatim"
        );
        let ContentBlock::Text(checkpoint_text) = &derived[2].1.content[0] else {
            panic!("checkpoint must be a text message");
        };
        assert!(checkpoint_text.contains("<compacted-summary>"));
        assert!(checkpoint_text.contains("SUMMARY-TEXT"));
        assert!(checkpoint_text.contains(starriver_compaction::CHECKPOINT_PREAMBLE));

 // summarizer parity: the summarize call reuses the step's system
        // prompt and the live surface as prefix; the instruction rides last.
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.system.as_deref(), Some("SYS"));
        assert_eq!(
            request.purpose,
            Some(starriver_kernel::llm::CallPurpose::Compaction)
        );
        assert_eq!(request.messages.len(), 9);
        let ContentBlock::Text(instruction) = &request.messages[8].content[0] else {
            panic!("last summarize message must be the instruction");
        };
        assert_eq!(instruction, starriver_compaction::COMPACTION_INSTRUCTION);

        let ends = compaction_events(&agent, "compaction/end");
        assert_eq!(ends.len(), 1);
        // ok flag rides in the end event's first pair.
        assert!(
            matches!(&ends[0], JsonValue::Object(pairs) if pairs.iter().any(|(k, v)| k == "ok" && *v == JsonValue::Bool(true))),
            "compaction/end must record success"
        );
        // The summary event carries the checkpoint Markdown + route facts so
        // UI bridges can surface the fold row without re-reading the log.
        let summaries = compaction_events(&agent, "compaction/summary");
        assert_eq!(summaries.len(), 1);
        assert!(
            matches!(&summaries[0], JsonValue::Object(pairs)
                if pairs.iter().any(|(k, v)| k == "summary" && v.as_str() == Some("## Primary Request and Intent\nSUMMARY-TEXT"))
                && pairs.iter().any(|(k, v)| k == "provider" && v.as_str() == Some("mock"))),
            "compaction/summary must carry summary + provider: {:?}",
            summaries[0]
        );
    }

    #[tokio::test]
    async fn compaction_never_splits_a_tool_pair() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let agent = compaction_agent("SUMMARY", false, Arc::clone(&requests));
        seed_user_message(&agent, &"x".repeat(1000));
        seed_user_message(&agent, &"x".repeat(1000));
        seed_tool_pair(&agent, "c1");

        // Boundary lands on the tool result; it must move back before the
        // assistant tool-call message so the pair stays whole in the tail.
        agent.compact_if_needed(None, 800, 0.8).await;

        let derived = agent.session.derive_messages_with_seqs().unwrap();
        assert_eq!(derived.len(), 3, "assistant + result + checkpoint");
        assert!(
            matches!(&derived[0].1.content[0], ContentBlock::ToolCall { .. }),
            "assistant tool call must remain in the tail"
        );
        assert!(
            matches!(&derived[1].1.content[0], ContentBlock::ToolResult { .. }),
            "tool result must remain beside its call"
        );
        assert!(
            matches!(&derived[2].1.content[0], ContentBlock::Text(t) if t.contains("<compacted-summary>"))
        );
    }

    #[tokio::test]
    async fn compaction_failure_leaves_surface_unchanged() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let agent = compaction_agent("ignored", true, Arc::clone(&requests));
        for _ in 0..8 {
            seed_user_message(&agent, &"x".repeat(1000));
        }
        let surface_events_before = agent
            .session
            .events()
            .into_iter()
            .filter(|event| event.surface_op.is_some())
            .count();

        agent.compact_if_needed(None, 1000, 0.8).await;

        let derived = agent.session.derive_messages().unwrap();
        assert_eq!(derived.len(), 8, "no checkpoint on summarize failure");
        assert_eq!(
            agent
                .session
                .events()
                .into_iter()
                .filter(|event| event.surface_op.is_some())
                .count(),
            surface_events_before,
            "surface must not change"
        );
        assert!(compaction_events(&agent, "compaction/summary").is_empty());
        let ends = compaction_events(&agent, "compaction/end");
        assert!(
            matches!(&ends[0], JsonValue::Object(pairs) if pairs.iter().any(|(k, v)| k == "ok" && *v == JsonValue::Bool(false))),
            "compaction/end must record the failure"
        );
    }

    #[tokio::test]
    async fn compaction_skips_below_threshold() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let agent = compaction_agent("unused", false, Arc::clone(&requests));
        for _ in 0..8 {
            seed_user_message(&agent, &"x".repeat(1000));
        }

        agent.compact_if_needed(None, 1_000_000, 0.8).await;

        assert!(requests.lock().unwrap().is_empty(), "no summarize call");
        assert!(compaction_events(&agent, "compaction/start").is_empty());
        assert_eq!(agent.session.derive_messages().unwrap().len(), 8);
    }
}
