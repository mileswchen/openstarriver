//! Tools seam: scoped tool registry and the guarded execution pipeline.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cordis_rslite::{BoxFuture, EffectHandle, EventError, Service, Value};
use tokio_util::sync::CancellationToken;

use crate::llm::{CallId, ContentBlock, Message, ToolSchema};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToolExecutionToken(u64);

static NEXT_TOOL_EXECUTION_TOKEN: AtomicU64 = AtomicU64::new(0);

/// Mint a unique opaque execution token, mirroring
/// `Symbol('dsh.tool.execution')`: identity is the value, the payload is
/// unreadable, and uniqueness is guaranteed by the counter.
pub fn mint_tool_execution_token() -> ToolExecutionToken {
    ToolExecutionToken(NEXT_TOOL_EXECUTION_TOKEN.fetch_add(1, Ordering::Relaxed))
}

#[derive(Debug, Clone)]
pub struct ToolRunContext {
    pub execution: ToolExecution,
    deferred: Arc<Mutex<Vec<Message>>>,
    concludes: Arc<AtomicBool>,
}

impl ToolRunContext {
    pub fn new(execution: ToolExecution) -> Self {
        Self {
            execution,
            deferred: Arc::new(Mutex::new(Vec::new())),
            concludes: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancellation(&self) -> Option<CancellationToken> {
        self.execution.cancellation.clone()
    }

    /// Defer one context — typically a nested-dispatch context ferried by a
    /// composite tool, or a fresh plugin-sourced instruction — until this
 /// tool's final result reaches the agent loop, matching
    /// `ToolRunContext.deferContext`.
    pub fn defer_context(&self, message: Message) {
        self.deferred
            .lock()
            .expect("tool deferred contexts poisoned")
            .push(message);
    }

    /// Mark a successful final result as terminal for the current agent turn,
 /// matching `ToolRunContext.concludeTurn`.
    pub fn conclude_turn(&self) {
        self.concludes.store(true, Ordering::SeqCst);
    }

    pub fn takes_deferred(&self) -> Vec<Message> {
        let mut deferred = self
            .deferred
            .lock()
            .expect("tool deferred contexts poisoned");
        std::mem::take(&mut *deferred)
    }

    pub fn concludes_turn(&self) -> bool {
        self.concludes.load(Ordering::SeqCst)
    }
}

pub trait ToolExecutor: Send + Sync {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>>;
}

/// Per-call concurrency classifier: only an exact `true` opts in.
pub type ToolConcurrencyClassifier = Arc<dyn Fn(&Value) -> bool + Send + Sync>;

/// Pure projection from validated arguments and canonical value to
/// model-facing content, matching `ToolOutputDefinition.render`.
pub type ToolRenderer = Arc<dyn Fn(&Value, &Value) -> Vec<ContentBlock> + Send + Sync>;

/// Definition-owned last-mile content transform snapshotted at execution
/// start and invoked exactly once immediately before materialization,
/// matching `ToolDefinition.finalizeContent`.
pub type ToolContentFinalizer =
    Arc<dyn Fn(&ToolExecution, &ToolExecutionResult) -> Option<Vec<ContentBlock>> + Send + Sync>;

/// Pure presentation-metadata projection (`output.presentationMeta`
/// parity): `(args, value) → opaque JSON`, persisted verbatim on the
/// `tool/result` event as `meta` and re-readable on log replay. Two rules
/// are part of the contract: the function must be PURE (only its inputs —
/// replay calls it with old args) and must SOFT-FAIL (`None`) on unexpected
/// shapes instead of panicking — the execute path is where hard failures
/// belong.
pub type ToolPresentationMeta =
    Arc<dyn Fn(&Value, &Value) -> Option<serde_json::Value> + Send + Sync>;

#[derive(Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub output: Value,
    /// Whether a pre-execute listener should ask the human before dispatch
    /// (host product policy decides the actual Ask; this is the tool-level
    /// fact that a product's trigger listens for).
    pub requires_approval: bool,
    /// Whether the tool writes files and must be refused in a read-only
    /// sandbox before approval (product trigger policy reads this fact).
    pub deny_in_readonly: bool,
    /// Mandatory canonical output projection from the body's returned value.
    pub render: ToolRenderer,
    pub executor: Arc<dyn ToolExecutor>,
    /// Per-call concurrency classification used by the loop scheduler. Only an
    /// exact `true` opts in; omission, exceptions, and non-`true` returns are
 /// fail-closed exclusive, matching `isConcurrencySafe`.
    pub is_concurrency_safe: Option<ToolConcurrencyClassifier>,
    /// Cooperative per-tool timeout budget enforced by a `tools/execute`
    /// waterfall wrapper policy; never enforced by the registry itself and
    /// never sent to the model.
    pub timeout: Option<Duration>,
    /// Optional final content transform applied to every normalized outcome,
    /// including pipeline failures that bypass `tools/post-execute`.
    pub finalize_content: Option<ToolContentFinalizer>,
}

#[derive(Debug, Clone)]
pub struct ToolExecutionInput {
    pub call_id: CallId,
    pub root_call_id: Option<CallId>,
    pub name: String,
    pub arguments: Value,
    pub agent_id: Option<String>,
    /// Immutable session working directory (the sandbox workspace boundary).
    pub cwd: Option<std::path::PathBuf>,
    /// Effective sandbox mode folded from the session log at execution start.
    pub sandbox_mode: Option<crate::sandbox::SandboxMode>,
    pub parent: Option<ToolExecutionToken>,
    pub cancellation: Option<CancellationToken>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Parallel,
    Exclusive,
}

#[derive(Debug, Clone)]
pub struct CodeDispatchLog {
    pub execution: ToolExecution,
    pub agent_id: Option<String>,
    pub sub_call_id: CallId,
    pub name: String,
    pub is_error: bool,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone)]
pub struct ToolExecution {
    pub call_id: CallId,
    pub root_call_id: CallId,
    pub name: String,
    pub arguments: Value,
    pub agent_id: Option<String>,
    /// Immutable session working directory (the sandbox workspace boundary).
    pub cwd: Option<std::path::PathBuf>,
    /// Effective sandbox mode folded from the session log at execution start.
    pub sandbox_mode: Option<crate::sandbox::SandboxMode>,
    pub parent: Option<ToolExecutionToken>,
    pub token: ToolExecutionToken,
    pub cancellation: Option<CancellationToken>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreToolDecision {
    Allow,
    Deny { reason: String },
    Ask { reason: Option<String> },
}

#[derive(Debug, Clone)]
pub struct ToolFailure {
    pub message: String,
    pub code: String,
}

#[derive(Debug, Clone)]
pub enum ToolExecutionResult {
    Success {
        value: Value,
        content: Vec<ContentBlock>,
        additional_contexts: Vec<Message>,
        concludes_turn: bool,
    },
    Failure {
        error: ToolFailure,
        content: Vec<ContentBlock>,
        additional_contexts: Vec<Message>,
    },
}

impl ToolExecutionResult {
    pub fn is_error(&self) -> bool {
        matches!(self, ToolExecutionResult::Failure { .. })
    }

    pub fn content(&self) -> &[ContentBlock] {
        match self {
            ToolExecutionResult::Success { content, .. }
            | ToolExecutionResult::Failure { content, .. } => content,
        }
    }

    pub fn error(&self) -> Option<&ToolFailure> {
        match self {
            ToolExecutionResult::Success { .. } => None,
            ToolExecutionResult::Failure { error, .. } => Some(error),
        }
    }

    pub fn additional_contexts(&self) -> &[Message] {
        match self {
            ToolExecutionResult::Success {
                additional_contexts,
                ..
            }
            | ToolExecutionResult::Failure {
                additional_contexts,
                ..
            } => additional_contexts,
        }
    }

    pub fn concludes_turn(&self) -> bool {
        matches!(
            self,
            ToolExecutionResult::Success {
                concludes_turn: true,
                ..
            }
        )
    }
}

#[derive(Debug, Clone)]
pub enum PostToolDecision {
    Accept {
        content: Option<Vec<ContentBlock>>,
        value: Option<Value>,
        additional_contexts: Vec<Message>,
    },
    Block {
        feedback: Vec<ContentBlock>,
        additional_contexts: Vec<Message>,
    },
}

/// Prepare-stage outcome: the next scheduler stage for one execution.
#[derive(Debug, Clone)]
pub enum ScheduledToolPreparation {
    /// The body stage still runs (around-dispatch waterfall + executor).
    Dispatch { exec: ToolRunContext },
    /// Skip dispatch; the result still receives post-execute.
    PostResult {
        exec: ToolRunContext,
        result: ToolExecutionResult,
    },
    /// Terminal result that bypasses both dispatch and post-execute.
    FinalResult {
        exec: ToolRunContext,
        result: ToolExecutionResult,
    },
}

/// Dispatch-stage outcome consumed by `finalize`/`finish`.
#[derive(Debug, Clone)]
pub enum ScheduledToolDispatch {
    PostResult { result: ToolExecutionResult },
    FinalResult { result: ToolExecutionResult },
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tool `{0}` is not registered")]
    NotFound(String),
    #[error("tool `{0}` is already registered")]
    AlreadyRegistered(String),
    #[error("tool `{tool}` execution failed: {message}")]
    Execution {
        tool: String,
        code: String,
        message: String,
    },
    #[error("tool `{0}` denied: {1}")]
    Denied(String, String),
    #[error("tool `{tool}` returned invalid output: {violations:?}")]
    InvalidOutput {
        tool: String,
        violations: Vec<String>,
    },
    #[error("tool pipeline event failed: {0}")]
    Event(#[from] EventError),
}

impl ToolError {
    /// Canonical failure code for durable diagnostics and policy routing,
 /// mirroring `ToolErrorInfo.code`.
    pub fn code(&self) -> &str {
        match self {
            ToolError::NotFound(_) => "UNKNOWN_TOOL",
            ToolError::AlreadyRegistered(_) => "ALREADY_REGISTERED",
            ToolError::Execution { code, .. } => code,
            ToolError::Denied(..) => "DENIED",
            ToolError::InvalidOutput { .. } => "INVALID_TOOL_OUTPUT",
            ToolError::Event(_) => "EVENT",
        }
    }
}

/// Product seam for scope-aware tool registration and execution.
pub trait ToolRuntime: Service {
    const NAME: &'static str = "tools";

    fn register(&self, definition: ToolDefinition) -> Result<EffectHandle, ToolError>;
    /// Register a monotonic execution guard evaluated after every
    /// `tools/pre-execute` listener and before the tool body. Any matching
    /// guard may deny; no guard can force-allow a call another guard denied.
    fn guard(&self, guard: ToolGuard) -> Result<EffectHandle, ToolError>;
    /// Classify a pending call for the agent-loop scheduler: only an exact
    /// `true` from the tool's classifier is parallel; everything else is
    /// fail-closed exclusive.
    fn execution_mode(&self, input: &ToolExecutionInput) -> ToolExecutionMode;
 /// Presentation metadata for one settled call (
    /// `output.presentationMeta` parity): the producing tool's pure
    /// `(args, value) → opaque JSON` projection. `None` when the tool
    /// declares no presenter or the call is a sub-dispatch
    /// (`root_call_id` set) — metadata attaches to top-level `tool/result`
    /// events only.
    fn presentation_meta(
        &self,
        input: &ToolExecutionInput,
        value: &Value,
    ) -> Option<serde_json::Value> {
        let _ = (input, value);
        None
    }
    /// Run the `tools/code-dispatch-log` waterfall over one settled
    /// sub-dispatch and return the content to log. Contained: a throwing
    /// listener falls back to the original settled content and never fails
    /// the dispatch.
    fn shape_dispatch_log(&self, dispatch: CodeDispatchLog) -> BoxFuture<Vec<ContentBlock>>;
    /// Snapshot the currently registered tool definitions as model-visible
    /// schemas, in registration order. The agent loop presents these to the
    /// model each step; agent-scoped tool filtering is a later phase.
    fn schemas(&self) -> Vec<ToolSchema>;
    /// Snapshot the model-visible tool schemas for one agent. `None` or an
    /// unknown agent returns the global set; an agent with a whitelist gets
 /// the intersection (global ∩ allow). Aligns with `schemas(scope)`.
    fn schemas_for(&self, _agent_id: Option<&str>) -> Vec<ToolSchema> {
        self.schemas()
    }
    /// Catalog for the PTC program-toolchain prompt section: the tools a
    /// program may call, EXCLUDING the reserved `run_code` transport itself.
    /// Under PTC mode this catalog renders while `schemas()` presents only
    /// `run_code`; the two projections together are the prompt-side half of
    /// the ptc-only contract.
    fn sdk_schemas(&self) -> Vec<ToolSchema> {
        self.schemas().into_iter().filter(|s| s.name != "run_code").collect()
    }
    /// Whether PTC mode presentation and enforcement are currently active.
    fn is_ptc_active(&self) -> bool {
        false
    }
    /// Restrict global tools for one agent scope: `allow` keeps only the
    /// named tools; later restrictions intersect. Returns a disposer that
 /// lifts it. Aligns with `tools.restrict(allow)`.
    fn restrict_for(&self, agent_id: &str, allow: Vec<String>) -> Result<EffectHandle, ToolError>;
    /// Staged scheduler surface consumed by the agent loop. `prepare`
    /// materializes the execution and runs the pre-execute/guard gate;
    /// `dispatch` runs only the around-dispatch/body stage; `finalize` runs
    /// post-execute then `finish`; `finish` applies content finalization,
    /// materializes, and notifies `tools/result`. Ordinary callers use
    /// [`ToolRuntime::execute`].
    fn prepare(
        &self,
        input: ToolExecutionInput,
    ) -> cordis_rslite::BoxFuture<Result<ScheduledToolPreparation, ToolError>>;
    fn dispatch(
        &self,
        exec: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<ScheduledToolDispatch, ToolError>>;
    fn finalize(
        &self,
        exec: ToolRunContext,
        result: ToolExecutionResult,
    ) -> cordis_rslite::BoxFuture<Result<ToolExecutionResult, ToolError>>;
    fn finish(
        &self,
        exec: ToolRunContext,
        result: ToolExecutionResult,
    ) -> Result<ToolExecutionResult, ToolError>;
    fn execute(
        &self,
        input: ToolExecutionInput,
    ) -> cordis_rslite::BoxFuture<Result<ToolExecutionResult, ToolError>>;
}

/// Monotonic pre-body check: a returned `Some(reason)` denies the execution.
pub type ToolGuard = Arc<dyn Fn(&ToolExecution) -> Option<String> + Send + Sync>;

cordis_rslite::cordis_event!(ToolsPreExecute, "tools/pre-execute", {
    execution: ToolExecution,
});

impl cordis_rslite::WaterfallEvent for ToolsPreExecute {
    type Output = PreToolDecision;
}

cordis_rslite::cordis_event!(ToolsExecute, "tools/execute", {
    execution: ToolExecution,
});

impl cordis_rslite::WaterfallEvent for ToolsExecute {
    type Output = ToolExecutionResult;
}

cordis_rslite::cordis_event!(ToolsPostExecute, "tools/post-execute", {
    execution: ToolExecution,
    result: ToolExecutionResult,
});

impl cordis_rslite::WaterfallEvent for ToolsPostExecute {
    type Output = PostToolDecision;
}

cordis_rslite::cordis_event!(ToolsResult, "tools/result", {
    execution: ToolExecution,
    result: ToolExecutionResult,
});

cordis_rslite::cordis_event!(ToolsCodeDispatchLog, "tools/code-dispatch-log", {
    dispatch: CodeDispatchLog,
});

impl cordis_rslite::WaterfallEvent for ToolsCodeDispatchLog {
    type Output = Vec<ContentBlock>;
}

cordis_rslite::cordis_event!(ToolsAvailable, "tools/available", {
    names: Vec<String>,
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Message, MessageId, MessageRole, MessageSource};

    fn sample_execution() -> ToolExecution {
        ToolExecution {
            cwd: None,
            sandbox_mode: None,
            call_id: CallId("c1".into()),
            root_call_id: CallId("c1".into()),
            name: "echo".into(),
            arguments: Value::new(1u64),
            agent_id: None,
            parent: None,
            token: mint_tool_execution_token(),
            cancellation: None,
        }
    }

    #[test]
    fn minted_tokens_are_unique() {
        let first = mint_tool_execution_token();
        let second = mint_tool_execution_token();
        assert_ne!(first, second);
    }

    #[test]
    fn run_context_defers_and_concludes() {
        let ctx = ToolRunContext::new(sample_execution());
        assert_eq!(ctx.execution.name, "echo");
        assert!(ctx.cancellation().is_none());
        assert!(!ctx.concludes_turn());
        assert!(ctx.takes_deferred().is_empty());

        ctx.defer_context(Message {
            id: MessageId("m1".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::Text("deferred".into())],
            source: MessageSource::User,
        });
        ctx.defer_context(Message {
            id: MessageId("m2".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::Text("deferred-2".into())],
            source: MessageSource::User,
        });
        ctx.conclude_turn();

        assert!(ctx.concludes_turn());
        let deferred = ctx.takes_deferred();
        assert_eq!(deferred.len(), 2);
        // `takes_deferred` drains the buffer; the conclude marker is sticky.
        assert!(ctx.takes_deferred().is_empty());
        assert!(ctx.concludes_turn());
    }

    #[test]
    fn tool_error_codes_are_canonical() {
        assert_eq!(ToolError::NotFound("x".into()).code(), "UNKNOWN_TOOL");
        assert_eq!(
            ToolError::AlreadyRegistered("x".into()).code(),
            "ALREADY_REGISTERED"
        );
        assert_eq!(
            ToolError::Execution {
                tool: "x".into(),
                code: "ABORTED".into(),
                message: "m".into(),
            }
            .code(),
            "ABORTED"
        );
        assert_eq!(ToolError::Denied("x".into(), "r".into()).code(), "DENIED");
        assert_eq!(
            ToolError::InvalidOutput {
                tool: "x".into(),
                violations: Vec::new(),
            }
            .code(),
            "INVALID_TOOL_OUTPUT"
        );
        assert_eq!(ToolError::Event(EventError::Timeout).code(), "EVENT");
    }

    #[test]
    fn execution_result_is_error_helper() {
        let success = ToolExecutionResult::Success {
            value: Value::new(1u64),
            content: Vec::new(),
            additional_contexts: Vec::new(),
            concludes_turn: false,
        };
        assert!(!success.is_error());
        let failure = ToolExecutionResult::Failure {
            error: ToolFailure {
                message: "m".into(),
                code: "X".into(),
            },
            content: Vec::new(),
            additional_contexts: Vec::new(),
        };
        assert!(failure.is_error());
    }

    #[test]
    fn tool_definition_clones_share_classifier_and_executor() {
        let definition = ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: "echo".into(),
            description: "echo".into(),
            parameters: Value::new(()),
            output: Value::new(()),
            render: Arc::new(|_, _| Vec::new()),
            executor: Arc::new(NoopExecutor),
            is_concurrency_safe: Some(Arc::new(|_| true)),
            timeout: Some(Duration::from_millis(10)),
            finalize_content: Some(Arc::new(|_, _| None)),
        };
        let cloned = definition.clone();
        assert_eq!(cloned.name, "echo");
        assert!(cloned.is_concurrency_safe.is_some());
        assert_eq!(cloned.timeout, Some(Duration::from_millis(10)));
        assert!(cloned.finalize_content.is_some());
    }

    #[test]
    fn scheduled_preparation_and_dispatch_variants_construct() {
        let execution = sample_execution();
        let exec = ToolRunContext::new(execution);
        let result = ToolExecutionResult::Success {
            value: Value::new(()),
            content: Vec::new(),
            additional_contexts: Vec::new(),
            concludes_turn: false,
        };
        let cases = [
            format!(
                "{:?}",
                ScheduledToolPreparation::Dispatch { exec: exec.clone() }
            ),
            format!(
                "{:?}",
                ScheduledToolPreparation::PostResult {
                    exec: exec.clone(),
                    result: result.clone(),
                }
            ),
            format!(
                "{:?}",
                ScheduledToolPreparation::FinalResult {
                    exec: exec.clone(),
                    result: result.clone(),
                }
            ),
            format!(
                "{:?}",
                ScheduledToolDispatch::PostResult {
                    result: result.clone()
                }
            ),
            format!("{:?}", ScheduledToolDispatch::FinalResult { result }),
        ];
        assert!(cases.iter().any(|case| case.contains("Dispatch")));
        assert!(cases.iter().any(|case| case.contains("PostResult")));
        assert!(cases.iter().any(|case| case.contains("FinalResult")));
    }

    struct NoopExecutor;
    impl ToolExecutor for NoopExecutor {
        fn execute(
            &self,
            _arguments: Value,
            _ctx: ToolRunContext,
        ) -> BoxFuture<Result<Value, ToolError>> {
            Box::pin(async { Ok(Value::new(())) })
        }
    }
}
