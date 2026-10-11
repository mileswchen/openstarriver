//! Agent seam: loop-agnostic agent interface, registry contract, `agent/*`
//! vocabulary, and the durable inbox projection.
//!
//! Plugins and UI program against this seam; the concrete driver lives in the
//! `starriver-agent-loop` crate and is swappable.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use cordis_rslite::{BoxFuture, Context, EffectHandle, Service};
use tokio_util::sync::CancellationToken;

use crate::json::JsonValue;
use crate::llm::{ContentBlock, LlmCallConfig, LlmFailure, Message, MessageId, RetryPolicy};
use crate::session::{Session, SessionId};

/// Live driver status exposed to consumers. `Running` is a driver-wide drain
/// interval, not proof that a turn is still open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Running,
}

/// Loop options owned by the agent handle.
#[derive(Debug, Clone, Default)]
pub struct AgentOptions {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub max_tokens: Option<u64>,
    /// Legacy whole-string system prompt; used when system_sections is empty.
    pub system: Option<String>,
    /// Static prompt sections (name, text); merged in section order when
    /// non-empty, taking precedence over `system`.
    pub system_sections: Vec<(String, String)>,
    /// Per-agent visible tool whitelist; None = all global tools visible.
    pub visible_tools: Option<Vec<String>>,
    /// Prompt composition toggles (LLP settings namespace). `None` = all
    /// defaults (identity on, runtime-context on, no source section, no
    /// global persona). Products set this to enable/disable residents.
    pub prompt_settings: Option<crate::system_prompt::PromptSettings>,
    /// Time-context sampling (SP4): None disables the snapshot entirely;
    /// `Some(ms)` enables it with a minimum interval between samples
 /// (`Some(0)` = attempt every step, diff-suppressed — the `None`
    /// semantics). The product preset ships `Some(60_000)`.
    pub time_refresh_interval_ms: Option<u64>,
    /// Context window for compaction threshold detection. `None` = no
    /// compaction (default). Products set this from the model's spec.
    pub context_window: Option<u64>,
    /// Compaction trigger ratio (fraction of context_window). Default 0.8.
    pub compaction_threshold_ratio: Option<f64>,
 /// WI-10: compaction retain ratio (retainRatio). None = engine
    /// default (0.16).
    pub compaction_retain_ratio: Option<f64>,
    /// Per-step parallel tool-call budget (backlog #7 settings seam). `None`
    /// = the loop's default (10); the product preset fills it from
    /// `agent.max_parallel_tool_calls` settings so a settings write takes
    /// effect on the next run without a restart.
    pub max_parallel_tool_calls: Option<usize>,
    /// BUG-138 repeat breaker: total non-reload tool calls allowed per turn.
    /// `None` = the loop's default (666); filled from
    /// `agent.turn_tool_call_budget` settings (same next-run semantics).
    pub turn_tool_call_budget: Option<u32>,
    /// WI-1: image cold/hot projection config. `None` = projection disabled
    /// (Image blocks pass through exactly as logged).
    pub image_projection: Option<ImageProjectionConfig>,
    /// WI-1: resolver the projection engine uses to turn `sha256:` file ids
    /// back into data URLs (hot) and summaries (cold). `None` = disabled.
    pub image_resolver: Option<SharedImageResolver>,
    /// WI-163: live MCP availability feed — snapshot of configured servers
    /// whose tools are NOT currently registered, `(server, failure detail)`.
    /// `None` disables the availability notice entirely.
    pub mcp_status_feed: Option<SharedMcpStatusFeed>,
    /// WI-9: compaction tuning knobs (pruner budgets + retry counts);
 /// `None` fields keep the engine defaults (compaction-basic).
    pub compaction_tuning: Option<CompactionTuning>,
}

/// WI-9: compaction tuning knobs (compaction-basic + tool-result-pruner
/// configuration parity). All `Option` — unset fields keep engine defaults.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompactionTuning {
    /// Pruner: trim a tool result when its combined text exceeds this many
 /// Unicode code points (thresholdChars, default 8192).
    pub prune_threshold_chars: Option<usize>,
 /// Pruner: leading code points retained (headChars, default 4096).
    pub prune_head_chars: Option<usize>,
 /// Pruner: trailing code points retained (tailChars, default 1024).
    pub prune_tail_chars: Option<usize>,
    /// Extra condense attempts when pressure remains after one condense
 /// (compactionRetries, default 1).
    pub compaction_retries: Option<u32>,
 /// Retries after a confirmed context-window overflow (
    /// maxOverflowRetries, default 1; 0 disables recovery).
    pub max_overflow_retries: Option<u32>,
}

/// WI-1/WI-159: cold/hot projection knobs (`maxImagesPerRequest` /
/// `maxRequestFilesBytes` budget parity — byte-budgeted, no fixed window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageProjectionConfig {
    /// Count cap on hot images per request (reload-pulls included). Secondary
    /// guard behind the byte budget.
    pub max_images: usize,
    /// Byte budget for hot images per request, counted in base64 wire length
    /// (what actually rides the request ≈ 4/3 of the normalized payload). 0
    /// together with `max_images: 0` is the text-only stamp.
    pub max_request_bytes: u64,
    /// WI-152 S2: whether `reload_image` pulls may force images hot. Text-only
    /// runs (hot override `(0, 0)`) disable reload: a pulled image would put
    /// image data on the wire, and cold placeholders must not advertise it.
    pub allow_reload: bool,
}

impl ImageProjectionConfig {
    /// Engine default for `max_images` — settings seams read this so the
    /// documented default lives next to the struct.
    pub fn default_max_images() -> usize {
        Self::default().max_images
    }
    /// Engine default for `max_request_bytes` (4 MiB base64 wire length).
    pub fn default_max_request_bytes() -> u64 {
        Self::default().max_request_bytes
    }
}

impl Default for ImageProjectionConfig {
    fn default() -> Self {
        Self {
            max_images: 8,
            max_request_bytes: 4 * 1024 * 1024,
            allow_reload: true,
        }
    }
}

/// Resolve stored image references for the cold/hot projection engine
/// (WI-1). The loop stays attachment-store-agnostic: hosts bridge their
/// asset library through this trait.
pub trait ImageResolver: Send + Sync {
    /// Data URL for a stored `sha256:<hex>` file id — the hot form sent to
    /// the model. `None` = unresolvable (block is treated as cold).
    fn resolve(&self, file_id: &str) -> Option<String>;
    /// Normalized payload byte length for the request budget (WI-159).
    /// `None` = unknown; the image then only counts against the image-count
    /// cap. The default keeps test/bridge resolvers budget-agnostic.
    fn byte_len(&self, _file_id: &str) -> Option<u64> {
        None
    }
}

/// `Arc<dyn ImageResolver>` wrapper with the derives `AgentOptions` needs
/// (trait objects are neither Debug nor Clone on their own).
#[derive(Clone)]
pub struct SharedImageResolver(pub Arc<dyn ImageResolver>);

impl std::fmt::Debug for SharedImageResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedImageResolver(<dyn>)")
    }
}

/// WI-163: live MCP availability snapshot — `(server name, failure detail)`
/// for every configured MCP server currently NOT registered (discovery
/// failed at startup/retry, or supervision marked it dead). Hosts bridge
/// their runtime state through this closure; the loop turns changes into a
/// pre-step notice so the model never plans around tools that silently
/// vanished.
pub type McpStatusFeed = Arc<dyn Fn() -> Vec<(String, String)> + Send + Sync>;

/// `Arc<dyn McpStatusFeed>` wrapper with the derives `AgentOptions` needs
/// (trait objects are neither Debug nor Clone on their own).
#[derive(Clone)]
pub struct SharedMcpStatusFeed(pub McpStatusFeed);

impl std::fmt::Debug for SharedMcpStatusFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedMcpStatusFeed(<dyn>)")
    }
}

/// Runtime mapping of agent_id → agent-scope Context. Tool events are routed
/// via `execution.agent_id` through this table to the corresponding agent's
/// ctx; on a miss or when no agent_id is present, the tool runtime's own ctx
/// is used (global event).
#[derive(Default, Clone)]
pub struct AgentScopeRegistry {
    inner: Arc<Mutex<HashMap<String, Context>>>,
}

impl AgentScopeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, agent_id: String, ctx: Context) {
        self.inner.lock().unwrap().insert(agent_id, ctx);
    }

    pub fn unregister(&self, agent_id: &str) {
        self.inner.lock().unwrap().remove(agent_id);
    }

    pub fn lookup(&self, agent_id: &str) -> Option<Context> {
        self.inner.lock().unwrap().get(agent_id).cloned()
    }
}

/// Cancellation cause recorded on `turn/end` for an aborted turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCancelKind {
    User,
    Parent,
    Disposed,
}

#[derive(Debug, Clone)]
pub struct AgentCancelCause {
    pub kind: AgentCancelKind,
    pub reason: Option<String>,
}

impl AgentCancelCause {
    pub fn new(kind: AgentCancelKind) -> Self {
        Self { kind, reason: None }
    }

    /// `new` plus a free-form provenance tag for the cancel source (call
    /// site / API surface). Surfaces in turn/end (`source`) so an aborted
    /// turn can be attributed after the fact.
    pub fn with_reason(kind: AgentCancelKind, reason: impl Into<String>) -> Self {
        Self { kind, reason: Some(reason.into()) }
    }
}

/// The two ordered pending-message lists owned by an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxTarget {
    NextTurn,
    NextStep,
}

/// How a published agent started, matching `SessionStartSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStartSource {
    Startup,
    Resume,
}

/// Pre-step gate decision: enter the proposed batch or reject the step.
#[derive(Debug, Clone)]
pub enum PreStepDecision {
    Enter { messages: Vec<Message> },
    Reject,
}

/// Failed-model-request recovery decision from the `agent/request-error`
/// waterfall: `Retry` keeps the step running; `None` is terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestErrorAction {
    Retry,
    None,
}

/// Read-only snapshot of an agent's pending inbox messages.
#[derive(Debug, Clone, Default)]
pub struct PendingMessages {
    pub next_turn: Vec<Message>,
    pub next_step: Vec<Message>,
}

/// The handle every plugin programs against. The concrete loop implements it.
pub trait Agent: Send + Sync {
    fn id(&self) -> &SessionId;
    fn options(&self) -> &AgentOptions;
    fn session(&self) -> &Session;
    fn status(&self) -> AgentStatus;
    fn ctx(&self) -> &Context;

    fn followup(&self, message: Message);
    fn steer(&self, message: Message);
    fn inject(&self, message: Message);
    fn send(&self, message: Message, target: InboxTarget, wakeup: bool);
    /// Durably cancel the agent's live turn, if any.
    /// Returns `true` only when a running turn was actually aborted; `false`
    /// when the agent was idle (nothing to cancel). Callers surface this so a
    /// UI that armed a "stopping" affordance resets instead of waiting for a
    /// TaskCancelled that will never arrive.
    fn cancel(&self, cause: AgentCancelCause, keep_inbox: bool) -> bool;
    fn when_idle(&self) -> BoxFuture<()>;

    /// Manual compaction hook (`session.compact`): drivers that support
    /// summarize-and-replace compaction override this. The default `None`
    /// means this agent cannot compact on demand.
    fn force_compact(&self, system: Option<&str>, window: u64) -> BoxFuture<Option<(u64, u64)>> {
        let _ = (system, window);
        Box::pin(std::future::ready(None))
    }

    /// [`force_compact`] with the protocol's `keepRecent` cap on the
    /// verbatim tail; the default ignores the cap (drivers that support
    /// retention override).
    fn force_compact_keep_recent(
        &self,
        system: Option<&str>,
        window: u64,
        keep_recent: u32,
    ) -> BoxFuture<Option<(u64, u64)>> {
        let _ = keep_recent;
        self.force_compact(system, window)
    }

    /// Snapshot of the pending inbox messages, for UI/plugin reads. The
    /// default is empty; drivers override with their live projection.
    fn pending_messages(&self) -> PendingMessages {
        PendingMessages::default()
    }

    fn pending_remove(&self, id: &str) -> bool {
        let _ = id;
        false
    }

    fn pending_update_text(&self, id: &str, text: &str) -> bool {
        let _ = (id, text);
        false
    }

    /// Replace one queued message's whole content (host-side image admission
    /// rebuilds Text/Image blocks from the edited text). Default: unsupported.
    fn pending_replace(&self, id: &str, content: Vec<ContentBlock>) -> bool {
        let _ = (id, content);
        false
    }

    fn pending_flush(&self) {}

    /// Whether the durable queue is parked: a manual stop held it, so the
    /// driver must not auto-continue. Hosts read this to tell an
    /// intentionally held queue (safe to dispose) from a stranded follow-up
    /// turn (must keep waiting). Default: false — no parkable queue.
    fn is_parked(&self) -> bool {
        false
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("agent `{0}` is not registered")]
    NotFound(SessionId),
    #[error("agent `{0}` already exists")]
    AlreadyExists(SessionId),
    #[error("no agent factory is registered")]
    NoFactory,
    #[error("no initiating agent is active")]
    NoInitiator,
    #[error("session persistence is not configured")]
    NoPersistence,
    #[error("agent operation failed: {0}")]
    Other(String),
}

/// Consumer-owned teardown capability for one published agent.
pub struct AgentHandle {
    pub agent: Arc<dyn Agent>,
    dispose: Option<Box<dyn FnOnce() + Send>>,
}

impl AgentHandle {
    pub fn new(agent: Arc<dyn Agent>, dispose: Box<dyn FnOnce() + Send>) -> Self {
        Self {
            agent,
            dispose: Some(dispose),
        }
    }

    pub fn dispose(mut self) {
        if let Some(dispose) = self.dispose.take() {
            dispose();
        }
    }
}

impl Drop for AgentHandle {
    fn drop(&mut self) {
        if let Some(dispose) = self.dispose.take() {
            dispose();
        }
    }
}

/// Programmatic creation options for the factory path.
#[derive(Debug, Clone)]
pub struct CreateAgentOptions {
    pub session_id: SessionId,
    pub seed: Vec<crate::session::SessionEvent>,
    pub cwd: Option<String>,
 /// Agent preset id stamped into the fresh session header (binding:
    /// creation-locked, resolved against the loop's preset registry here and
    /// on every resume). `None` composes from the caller options alone.
    pub agent_preset: Option<String>,
    pub agent_options: AgentOptions,
}

/// Programmatic resume options for the factory path.
#[derive(Debug, Clone)]
pub struct ResumeAgentOptions {
    pub resume_session_id: SessionId,
    pub agent_options: AgentOptions,
    /// Workspace root stamped into the header when the id is unknown and the
    /// resume falls back to creation (the stored header already carries its
    /// own cwd on a true resume).
    pub cwd: Option<String>,
}

/// Agent creation backend registered via `AgentRegistry::set_factory`.
pub trait AgentFactory: Send + Sync {
    fn create(
        &self,
        owner: Context,
        options: CreateAgentOptions,
    ) -> BoxFuture<Result<AgentHandle, AgentError>>;
    fn resume(
        &self,
        owner: Context,
        options: ResumeAgentOptions,
    ) -> BoxFuture<Result<AgentHandle, AgentError>>;
}

/// Product seam for the live agent registry. The concrete implementation and
/// the loop driver live in `starriver-agent-loop`.
pub trait AgentRegistry: Service {
    fn register(&self, agent: Arc<dyn Agent>) -> Result<EffectHandle, AgentError>;
    fn enter(
        &self,
        agent: Arc<dyn Agent>,
        owner: Option<Arc<dyn Agent>>,
    ) -> Result<EffectHandle, AgentError>;
    fn announce(&self, agent: &Arc<dyn Agent>) -> Result<(), AgentError>;
    fn get(&self, id: &SessionId) -> Option<Arc<dyn Agent>>;
    fn list(&self) -> Vec<Arc<dyn Agent>>;
    fn roots(&self) -> Vec<Arc<dyn Agent>>;

    fn set_factory(&self, factory: Arc<dyn AgentFactory>) -> Result<EffectHandle, AgentError>;
    fn create(&self, options: CreateAgentOptions) -> BoxFuture<Result<AgentHandle, AgentError>>;
    fn resume(&self, options: ResumeAgentOptions) -> BoxFuture<Result<AgentHandle, AgentError>>;
}

// `Arc<dyn AgentRegistry>` must stay constructible — consumers hold the
// registry through this trait object directly (WI-62 retired the view shim).
const _: fn(std::sync::Arc<dyn AgentRegistry>) = |_| {};

cordis_rslite::cordis_event!(AgentCreated, "agent/created", {
    agent: Arc<dyn Agent>,
});

cordis_rslite::cordis_event!(AgentDisposed, "agent/disposed", {
    agent: Arc<dyn Agent>,
});

cordis_rslite::cordis_event!(AgentSessionStart, "agent/session-start", {
    agent: Arc<dyn Agent>,
    source: SessionStartSource,
});

cordis_rslite::cordis_event!(AgentStatusChanged, "agent/status", {
    agent: Arc<dyn Agent>,
    status: AgentStatus,
});

cordis_rslite::cordis_event!(AgentInboxInserted, "agent/inbox/inserted", {
    agent: Arc<dyn Agent>,
    message: Message,
});

cordis_rslite::cordis_event!(AgentInboxClaimed, "agent/inbox/claimed", {
    agent: Arc<dyn Agent>,
    message: Message,
    turn: u64,
});

cordis_rslite::cordis_event!(AgentInboxDiscarded, "agent/inbox/discarded", {
    agent: Arc<dyn Agent>,
    message: Message,
});

cordis_rslite::cordis_event!(AgentPreStep, "agent/pre-step", {
    turn: u64,
    step: u64,
    messages: Vec<Message>,
    signal: CancellationToken,
});

impl cordis_rslite::WaterfallEvent for AgentPreStep {
    type Output = PreStepDecision;
}

cordis_rslite::cordis_event!(AgentRequest, "agent/request", {
    turn: u64,
    step: u64,
    signal: CancellationToken,
});

impl cordis_rslite::WaterfallEvent for AgentRequest {
    type Output = LlmCallConfig;
}

cordis_rslite::cordis_event!(AgentRequestError, "agent/request-error", {
    turn: u64,
    step: u64,
    provider: String,
    failure: LlmFailure,
    retry_policy: Option<RetryPolicy>,
    signal: CancellationToken,
});

impl cordis_rslite::WaterfallEvent for AgentRequestError {
    type Output = RequestErrorAction;
}

cordis_rslite::cordis_event!(AgentTurnStopping, "agent/turn-stopping", {
    turn: u64,
    signal: CancellationToken,
});

cordis_rslite::cordis_event!(AgentErrorEvent, "agent/error", {
    turn: u64,
    step: u64,
    error: String,
});

tokio::task_local! {
    static CURRENT_AGENT: Arc<dyn Agent>;
}

/// Run `operation` with `agent` as the ambient initiator of the current task.
/// Task-locals do not cross `tokio::spawn` boundaries; authoritative identity
/// must be passed explicitly at those edges.
pub async fn with_initiator<T>(agent: Arc<dyn Agent>, operation: impl Future<Output = T>) -> T {
    CURRENT_AGENT.scope(agent, operation).await
}

/// The ambient initiator inherited by the current task, if any.
pub fn current_initiator() -> Option<Arc<dyn Agent>> {
    CURRENT_AGENT.try_with(|agent| agent.clone()).ok()
}

/// Require an ambient initiator, or fail when the current task has none.
pub fn require_initiator() -> Result<Arc<dyn Agent>, AgentError> {
    current_initiator().ok_or(AgentError::NoInitiator)
}

/// Live per-message notifications committed by inbox mutations.
#[derive(Clone)]
pub struct InboxNotifications {
    pub inserted: Arc<dyn Fn(Message) + Send + Sync>,
    pub discarded: Arc<dyn Fn(Message) + Send + Sync>,
    pub claimed: Arc<dyn Fn(Message, u64) + Send + Sync>,
}

impl InboxNotifications {
    pub fn silent() -> Self {
        Self {
            inserted: Arc::new(|_| {}),
            discarded: Arc::new(|_| {}),
            claimed: Arc::new(|_, _| {}),
        }
    }
}

/// Incremental durable projection of `agent/inbox/spliced` events. Every
/// mutation commits the durable event before mutating the live projection.
/// The Rust event bus dispatches asynchronously, so observers reconstruct
/// state from the durable event plus the live inserted/discarded
/// notifications rather than from a synchronous pre-splice read.
pub struct Inbox {
    next_turn: Vec<Message>,
    next_step: Vec<Message>,
    session: Session,
    notifications: InboxNotifications,
}

impl Inbox {
    pub fn new(session: Session, notifications: InboxNotifications) -> Self {
        let mut inbox = Self {
            next_turn: Vec::new(),
            next_step: Vec::new(),
            session,
            notifications,
        };
        // BUG-67: the projection is DURABLE — replay the whole log,
        // including the seed region (resume/fork sessions keep every event
        // below first_live_seq; skipping it silently dropped queued inputs
        // after a restart). Live-region events appended after construction
        // arrive through the notifications channel instead.
        let events = inbox.session.events();
        for event in events.iter() {
            if event.event_type == "agent/inbox/spliced" {
                inbox.apply(&event.data);
            }
        }
        inbox
    }

    pub fn next_turn(&self) -> &[Message] {
        &self.next_turn
    }

    pub fn next_step(&self) -> &[Message] {
        &self.next_step
    }

    pub fn has_pending(&self) -> bool {
        !self.next_turn.is_empty() || !self.next_step.is_empty()
    }

    /// Durably cancel all pending input, clearing next-step before next-turn.
    pub fn clear(&mut self) {
        self.splice(InboxTarget::NextStep, 0, self.next_step.len(), Vec::new());
        self.splice(InboxTarget::NextTurn, 0, self.next_turn.len(), Vec::new());
    }

    /// Remove and return the complete batch proposed for one step: all
    /// next-step input plus, at a turn boundary (`target == NextTurn`), one
    /// queued prompt. The durable splices are pure deletions without outcome.
    pub fn claim(&mut self, target: InboxTarget, turn: u64) -> Vec<Message> {
        let mut claimed = self.mutate(
            InboxTarget::NextStep,
            0,
            self.next_step.len(),
            Vec::new(),
            false,
        );
        if target == InboxTarget::NextTurn {
            claimed.extend(self.mutate(InboxTarget::NextTurn, 0, 1, Vec::new(), false));
        }
        for message in &claimed {
            (self.notifications.claimed)(message.clone(), turn);
        }
        claimed
    }

    pub fn append(&mut self, target: InboxTarget, message: Message) {
        let len = match target {
            InboxTarget::NextTurn => self.next_turn.len(),
            InboxTarget::NextStep => self.next_step.len(),
        };
        self.splice(target, len, 0, vec![message]);
    }

    pub fn prepend(&mut self, target: InboxTarget, message: Message) {
        self.splice(target, 0, 0, vec![message]);
    }

    /// Replace one pending message in place, possibly changing its identity.
    pub fn replace(&mut self, message_id: &MessageId, new_message: Message) -> bool {
        let Some((target, index)) = self.locate(message_id) else {
            return false;
        };
        self.splice(target, index, 1, vec![new_message]);
        true
    }

    /// Remove one pending message and durably record its cancellation.
    pub fn remove(&mut self, message_id: &MessageId) -> bool {
        let Some((target, index)) = self.locate(message_id) else {
            return false;
        };
        self.splice(target, index, 1, Vec::new());
        true
    }

    /// Apply standard splice semantics and durably record the normalized
    /// result. The durable event commits before the live projection mutates.
    pub fn splice(
        &mut self,
        target: InboxTarget,
        start: usize,
        delete_count: usize,
        inserted: Vec<Message>,
    ) -> Vec<Message> {
        self.mutate(target, start, delete_count, inserted, true)
    }

    fn locate(&self, message_id: &MessageId) -> Option<(InboxTarget, usize)> {
        if let Some(index) = self
            .next_turn
            .iter()
            .position(|message| message.id == *message_id)
        {
            return Some((InboxTarget::NextTurn, index));
        }
        self.next_step
            .iter()
            .position(|message| message.id == *message_id)
            .map(|index| (InboxTarget::NextStep, index))
    }

    fn apply(&mut self, data: &JsonValue) {
        let Some((target, start, removed_count, inserted)) = decode_splice(data) else {
            return;
        };
        let queue = match target {
            InboxTarget::NextTurn => &mut self.next_turn,
            InboxTarget::NextStep => &mut self.next_step,
        };
        if start > queue.len() || start + removed_count > queue.len() {
            return;
        }
        queue.splice(start..start + removed_count, inserted);
    }

    fn mutate(
        &mut self,
        target: InboxTarget,
        start: usize,
        delete_count: usize,
        inserted: Vec<Message>,
        discard_removed: bool,
    ) -> Vec<Message> {
        let queue_len = match target {
            InboxTarget::NextTurn => self.next_turn.len(),
            InboxTarget::NextStep => self.next_step.len(),
        };
        let actual_start = start.min(queue_len);
        let actual_delete = delete_count.min(queue_len - actual_start);
        if actual_delete == 0 && inserted.is_empty() {
            return Vec::new();
        }
        self.validate_inserted(target, actual_start, actual_delete, &inserted);

        let mut entries = vec![
            (
                "target".into(),
                JsonValue::String(
                    match target {
                        InboxTarget::NextTurn => "next-turn",
                        InboxTarget::NextStep => "next-step",
                    }
                    .into(),
                ),
            ),
            ("start".into(), JsonValue::UInt(actual_start as u64)),
        ];
        if actual_delete > 0 {
            entries.push(("removedCount".into(), JsonValue::UInt(actual_delete as u64)));
        }
        entries.push((
            "inserted".into(),
            JsonValue::Array(
                inserted
                    .iter()
                    .map(|message| message.to_json().unwrap())
                    .collect(),
            ),
        ));
        if discard_removed && actual_delete > 0 {
            entries.push(("outcome".into(), JsonValue::String("canceled".into())));
        }
        self.session
            .append_with(
                "agent/inbox/spliced",
                JsonValue::Object(entries),
                crate::session::AppendOptions::default(),
            )
            .expect("inbox splice append failed");

        let removed = {
            let queue = match target {
                InboxTarget::NextTurn => &mut self.next_turn,
                InboxTarget::NextStep => &mut self.next_step,
            };
            queue
                .splice(
                    actual_start..actual_start + actual_delete,
                    inserted.iter().cloned(),
                )
                .collect::<Vec<_>>()
        };
        if discard_removed {
            for message in &removed {
                (self.notifications.discarded)(message.clone());
            }
        }
        for message in inserted {
            (self.notifications.inserted)(message);
        }
        removed
    }

    /// Message identities must stay unique across both pending lists,
 /// including the same queue's post-splice remainder (matching
    /// candidate-based validation).
    fn validate_inserted(
        &self,
        target: InboxTarget,
        start: usize,
        delete_count: usize,
        inserted: &[Message],
    ) {
        let queue = match target {
            InboxTarget::NextTurn => &self.next_turn,
            InboxTarget::NextStep => &self.next_step,
        };
        let other = match target {
            InboxTarget::NextTurn => &self.next_step,
            InboxTarget::NextStep => &self.next_turn,
        };
        let mut candidate = queue.clone();
        candidate.splice(start..start + delete_count, inserted.iter().cloned());
        let mut seen = std::collections::HashSet::new();
        for message in candidate.iter().chain(other.iter()) {
            if !seen.insert(message.id.clone()) {
                panic!("inbox splice would duplicate message id {}", message.id.0);
            }
        }
    }
}

fn decode_splice(data: &JsonValue) -> Option<(InboxTarget, usize, usize, Vec<Message>)> {
    let target = match data.as_str_at("target")? {
        "next-turn" => InboxTarget::NextTurn,
        "next-step" => InboxTarget::NextStep,
        _ => return None,
    };
    let start = data.get("start")?.as_u64()? as usize;
    let removed_count = data
        .get("removedCount")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0) as usize;
    let inserted = data
        .get("inserted")
        .and_then(JsonValue::as_array)
        .unwrap_or(&[])
        .iter()
        .cloned()
        .map(Message::from_json)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some((target, start, removed_count, inserted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_rslite::{Event, EventBus};

    struct StubAgent {
        id: SessionId,
        options: AgentOptions,
        session: Session,
        ctx: Context,
        status: AgentStatus,
    }

    impl Agent for StubAgent {
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
            self.status
        }
        fn ctx(&self) -> &Context {
            &self.ctx
        }
        fn followup(&self, _message: Message) {}
        fn steer(&self, _message: Message) {}
        fn inject(&self, _message: Message) {}
        fn send(&self, _message: Message, _target: InboxTarget, _wakeup: bool) {}
        fn cancel(&self, _cause: AgentCancelCause, _keep_inbox: bool) -> bool { false }
        fn when_idle(&self) -> BoxFuture<()> {
            Box::pin(async {})
        }
    }

    fn stub_agent() -> Arc<StubAgent> {
        Arc::new(StubAgent {
            id: SessionId("a1".into()),
            options: AgentOptions::default(),
            session: Session::new(SessionId("a1".into()), EventBus::new()),
            ctx: Context::new(),
            status: AgentStatus::Idle,
        })
    }

    #[tokio::test]
    async fn initiator_scope_is_ambient_within_the_task() {
        let agent = stub_agent();
        assert!(current_initiator().is_none());
        let seen = with_initiator(Arc::clone(&agent) as Arc<dyn Agent>, async {
            require_initiator().unwrap().id().0 == "a1"
        })
        .await;
        assert!(seen);
        assert!(current_initiator().is_none());
    }

    #[test]
    fn event_names_are_stable() {
        assert_eq!(AgentCreated::NAME, "agent/created");
        assert_eq!(AgentDisposed::NAME, "agent/disposed");
        assert_eq!(AgentSessionStart::NAME, "agent/session-start");
        assert_eq!(AgentStatusChanged::NAME, "agent/status");
        assert_eq!(AgentInboxInserted::NAME, "agent/inbox/inserted");
        assert_eq!(AgentInboxClaimed::NAME, "agent/inbox/claimed");
        assert_eq!(AgentInboxDiscarded::NAME, "agent/inbox/discarded");
        assert_eq!(AgentPreStep::NAME, "agent/pre-step");
        assert_eq!(AgentRequest::NAME, "agent/request");
        assert_eq!(AgentRequestError::NAME, "agent/request-error");
        assert_eq!(AgentTurnStopping::NAME, "agent/turn-stopping");
        assert_eq!(AgentErrorEvent::NAME, "agent/error");
    }

    fn message(id: &str) -> Message {
        Message {
            id: MessageId(id.into()),
            role: crate::llm::MessageRole::User,
            content: vec![crate::llm::ContentBlock::Text("hi".into())],
            source: crate::llm::MessageSource::User,
        }
    }

    #[test]
    fn inbox_splice_and_claim_semantics() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        let mut inbox = Inbox::new(session, InboxNotifications::silent());
        let first = message("a");
        let second = message("b");
        inbox.append(InboxTarget::NextTurn, first);
        inbox.append(InboxTarget::NextStep, second);
        assert_eq!(inbox.next_turn().len(), 1);
        assert_eq!(inbox.next_step().len(), 1);
        assert!(inbox.has_pending());

        // Claim clears next-step plus one queued next-turn prompt.
        let claimed = inbox.claim(InboxTarget::NextTurn, 1);
        assert_eq!(claimed.len(), 2);
        assert!(!inbox.has_pending());
    }

    #[test]
    fn inbox_remove_durably_cancels_and_notifies() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let discarded = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&discarded);
        let notifications = InboxNotifications {
            inserted: Arc::new(|_| {}),
            discarded: Arc::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
            claimed: Arc::new(|_, _| {}),
        };
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        let mut inbox = Inbox::new(session, notifications);
        let pending = message("a");
        inbox.append(InboxTarget::NextTurn, pending.clone());
        assert!(inbox.remove(&pending.id));
        assert_eq!(discarded.load(Ordering::SeqCst), 1);

        let events = inbox.session.events();
        let splices = events
            .iter()
            .filter(|event| event.event_type == "agent/inbox/spliced")
            .count();
        assert_eq!(splices, 2);
    }

    #[test]
    fn inbox_replays_spliced_events() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        let mut first = Inbox::new(session.clone(), InboxNotifications::silent());
        first.append(InboxTarget::NextTurn, message("a"));
        drop(first);

        let replayed = Inbox::new(session, InboxNotifications::silent());
        assert_eq!(replayed.next_turn().len(), 1);
        assert_eq!(replayed.next_turn()[0].id.0, "a");
    }

    /// BUG-67 regression (round 5): the Inbox is a DURABLE projection — a
    /// resumed session (all events below first_live_seq, i.e. seeded) must
    /// reconstruct its pending inbox from the seeded spliced events. The
    /// replay skipped everything below first_live_seq, silently dropping
    /// queued user inputs after a restart.
    #[test]
    fn inbox_replays_seeded_events_on_resume() {
        // Build a live session with one queued message, then take its whole
        // event log as the seed of a "restarted" session.
        let bus = EventBus::new();
        let live = Session::new(SessionId("s1".into()), bus);
        let mut inbox = Inbox::new(live.clone(), InboxNotifications::silent());
        inbox.append(InboxTarget::NextTurn, message("queued"));
        drop(inbox);
        let seed = live.events();

        let restarted = Session::new(SessionId("s1".into()), EventBus::new())
            .with_seed(seed)
            .expect("seed");
        let replayed = Inbox::new(restarted, InboxNotifications::silent());
        assert_eq!(
            replayed.next_turn().len(),
            1,
            "a resumed session's durable inbox events must rebuild the pending queue"
        );
        assert_eq!(replayed.next_turn()[0].id.0, "queued");
    }

    #[test]
    #[should_panic(expected = "duplicate message id a")]
    fn inbox_rejects_same_queue_duplicate() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        let mut inbox = Inbox::new(session, InboxNotifications::silent());
        inbox.append(InboxTarget::NextTurn, message("a"));
        inbox.append(InboxTarget::NextTurn, message("b"));
        // Inserting a duplicate into the SAME queue must be rejected.
        inbox.splice(InboxTarget::NextTurn, 1, 0, vec![message("a")]);
    }

    #[test]
    fn inbox_replace_same_id_removes_occurrence() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        let mut inbox = Inbox::new(session, InboxNotifications::silent());
        inbox.append(InboxTarget::NextTurn, message("a"));
        // A replace whose delete covers the old occurrence keeps the id unique.
        inbox.splice(InboxTarget::NextTurn, 0, 1, vec![message("a")]);
        assert_eq!(inbox.next_turn().len(), 1);
    }

    #[test]
    #[should_panic(expected = "duplicate message id a")]
    fn inbox_rejects_cross_queue_duplicate() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        let mut inbox = Inbox::new(session, InboxNotifications::silent());
        inbox.append(InboxTarget::NextTurn, message("a"));
        // The same id cannot appear in the other pending list either.
        inbox.append(InboxTarget::NextStep, message("a"));
    }

    #[test]
    #[should_panic(expected = "duplicate message id b")]
    fn inbox_rejects_prepend_duplicate() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        let mut inbox = Inbox::new(session, InboxNotifications::silent());
        inbox.append(InboxTarget::NextTurn, message("a"));
        inbox.append(InboxTarget::NextTurn, message("b"));
        inbox.prepend(InboxTarget::NextTurn, message("b"));
    }

    #[test]
    fn inbox_duplicate_panic_leaves_no_durable_event() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        let mut inbox = Inbox::new(session.clone(), InboxNotifications::silent());
        inbox.append(InboxTarget::NextTurn, message("a"));
        inbox.append(InboxTarget::NextTurn, message("b"));

        let before = session.events().len();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            inbox.splice(InboxTarget::NextTurn, 1, 0, vec![message("a")]);
        }));
        assert!(result.is_err(), "duplicate splice must panic");
        // Validation happens before the durable append: no partial state.
        assert_eq!(session.events().len(), before);
        assert_eq!(inbox.next_turn().len(), 2);
        assert_eq!(inbox.next_turn()[0].id.0, "a");
        assert_eq!(inbox.next_turn()[1].id.0, "b");
    }

    #[test]
    fn inbox_replace_with_new_id_discards_old() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let discarded = Arc::new(AtomicUsize::new(0));
        let inserted = Arc::new(AtomicUsize::new(0));
        let discarded_counter = Arc::clone(&discarded);
        let inserted_counter = Arc::clone(&inserted);
        let notifications = InboxNotifications {
            inserted: Arc::new(move |_| {
                inserted_counter.fetch_add(1, Ordering::SeqCst);
            }),
            discarded: Arc::new(move |_| {
                discarded_counter.fetch_add(1, Ordering::SeqCst);
            }),
            claimed: Arc::new(|_, _| {}),
        };
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        let mut inbox = Inbox::new(session, notifications);
        inbox.append(InboxTarget::NextTurn, message("a"));

        assert!(inbox.replace(&MessageId("a".into()), message("c")));
        assert_eq!(inbox.next_turn().len(), 1);
        assert_eq!(inbox.next_turn()[0].id.0, "c");
        assert_eq!(discarded.load(Ordering::SeqCst), 1);
        assert_eq!(
            inserted.load(Ordering::SeqCst),
            2,
            "append + replace insert"
        );
    }

    #[test]
    fn agent_scope_registry_registers_lookup_and_unregisters() {
        let registry = AgentScopeRegistry::new();
        let ctx = Context::new();
        assert!(registry.lookup("a1").is_none());
        registry.register("a1".into(), ctx.clone());
        assert!(registry.lookup("a1").is_some());
        registry.unregister("a1");
        assert!(registry.lookup("a1").is_none());
    }

    #[test]
    fn agent_scope_registry_is_idempotent_and_isolated() {
        let registry = AgentScopeRegistry::new();
        // Unregistering an absent id is a no-op.
        registry.unregister("missing");
        // Distinct agents never alias.
        registry.register("a1".into(), Context::new());
        registry.register("a2".into(), Context::new());
        assert!(registry.lookup("a1").is_some());
        assert!(registry.lookup("a2").is_some());
        registry.unregister("a1");
        assert!(registry.lookup("a1").is_none());
        assert!(registry.lookup("a2").is_some());
        // Re-register after unregister works.
        registry.register("a1".into(), Context::new());
        assert!(registry.lookup("a1").is_some());
    }

    #[test]
    fn agent_scope_registry_replace_updates_context() {
        let registry = AgentScopeRegistry::new();
        let first = Context::new();
        let second = Context::new();
        registry.register("a1".into(), first);
        registry.register("a1".into(), second);
        // Last registration wins (single slot per agent).
        assert!(registry.lookup("a1").is_some());
    }

    #[test]
    fn agent_options_defaults_are_stable() {
        let options = AgentOptions::default();
        assert_eq!(options.system, None);
        assert!(options.system_sections.is_empty());
        assert_eq!(options.visible_tools, None);
    }
}
