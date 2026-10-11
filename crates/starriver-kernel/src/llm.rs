//! LLM seam: message/stream vocabulary, the adapter contract, and the
//! chunk-to-message assembler, aligned with the upstream assembler while
//! honoring Rust's closed-enum and `Value` constraints.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use cordis_rslite::{EffectHandle, EventBus, Service, WaterfallEvent};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::json::JsonValue;

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct MessageId(pub String);

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct CallId(pub String);

static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(0);

/// Mint a unique message id for assembled messages.
pub fn mint_message_id() -> MessageId {
    MessageId(format!(
        "msg-{}",
        NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

#[derive(Debug, Clone, PartialEq)]
pub struct LlmCallConfig {
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub stop: Option<Vec<String>>,
}

pub fn call_config_equals(a: &LlmCallConfig, b: &LlmCallConfig) -> bool {
    a.provider == b.provider
        && a.model == b.model
        && a.reasoning_effort == b.reasoning_effort
        && a.temperature == b.temperature
        && a.max_tokens == b.max_tokens
        && a.stop == b.stop
}

fn generate_options_matches_config(options: &GenerateOptions, config: &LlmCallConfig) -> bool {
    options.provider == config.provider
        && options.model == config.model
        && options.reasoning_effort == config.reasoning_effort
        && options.temperature == config.temperature
        && options.max_tokens == config.max_tokens
        && options.stop == config.stop
}

/// Effective config fields supplied by exact-model adapter resolution rather
/// than by the caller's request proposal, matching
/// `LlmCallConfigAdapterDefaults`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LlmCallConfigAdapterDefaults {
    pub reasoning_effort: bool,
    pub max_tokens: bool,
}

/// Provider-owned context capacity for one exact provider/model route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LlmModelContext {
    pub context_window: u64,
}

/// Display metadata for one registered provider route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmProviderInfo {
    pub id: String,
    pub name: String,
}

/// Merge-extensible provider model modality vocabulary (closed in Rust).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelModality {
    Text,
    Image,
}

/// One adapter-discovered model; catalog membership is advisory, not request
/// validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmModelInfo {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub input_modalities: Option<Vec<ModelModality>>,
}

/// Display metadata for one adapter-owned reasoning effort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmReasoningEffortInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// Adapter-owned reasoning capability for one exact model route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmReasoningInfo {
    pub efforts: Vec<LlmReasoningEffortInfo>,
    pub default_effort: Option<String>,
}

/// Exact-model metadata resolved by the owning adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmResolvedModelInfo {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub input_modalities: Option<Vec<ModelModality>>,
    pub context: Option<LlmModelContext>,
    pub default_max_tokens: Option<u64>,
    pub reasoning: Option<LlmReasoningInfo>,
}

/// One provider route an adapter plugin can activate through configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmConfigurableProvider {
    pub provider: String,
    pub display_name: String,
    pub settings_ns: String,
    pub settings_path: Vec<String>,
    pub declared: Option<bool>,
}

/// One interrogation of a provider endpoint that configuration has not stored
/// yet.
#[derive(Debug, Clone)]
pub struct LlmModelDiscoveryRequest {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub api: Option<String>,
    pub api_key: Option<String>,
    pub cancellation: Option<CancellationToken>,
}

/// One model an endpoint reports about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmDiscoveredModel {
    pub id: String,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
}

/// Model-discovery offer: interrogates one provider endpoint for the models
/// it advertises.
pub type LlmModelDiscovery = Arc<
    dyn Fn(
            LlmModelDiscoveryRequest,
        ) -> BoxFuture<'static, Result<Vec<LlmDiscoveredModel>, LlmError>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: JsonValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

/// Named contribution to a `snapshot`-form context, in assembly order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSnapshotSection {
    pub name: String,
    pub text: String,
}

/// Producer-declared context form, matching `ContextFormed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextForm {
    Instructions,
    Catalog,
    Snapshot {
        sections: Vec<ContextSnapshotSection>,
        /// WI-35: the composite exceeded the byte budget (a truncation notice
        /// was appended). Absent on pre-WI-35 snapshots = false/0.
        truncated: bool,
        budget_bytes: u64,
        composed_bytes: u64,
    },
    Notice {
        summary: String,
    },
    Relay,
    Recall,
}

#[derive(Debug, Clone)]
pub enum MessageSource {
    User,
    Plugin {
        plugin: String,
        form: Option<ContextForm>,
    },
    Model {
        provider: String,
        model: String,
        replay_state: Option<JsonValue>,
    },
    Tool {
        call_id: CallId,
    },
    Custom {
        kind: String,
        payload: JsonValue,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentBlockType {
    Text,
    Reasoning,
    Image,
    ToolCall,
    ToolResult,
    Custom(String),
}

#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text(String),
    Reasoning(String),
    Image {
        /// Durable raster image reference owned by the attachment seam.
        attachment: JsonValue,
    },
    ToolCall {
        id: CallId,
        name: Option<String>,
        /// WI-47 Phase B: lossless tree, parsed exactly once at ingress
        /// (adapter wire entry / BlockAssembler block completion). Malformed
        /// model text is preserved as `JsonValue::String(raw)`.
        arguments: JsonValue,
    },
    ToolResult {
        call_id: CallId,
        content: Vec<ContentBlock>,
        is_error: bool,
    },
    Custom {
        kind: String,
        payload: JsonValue,
    },
}

impl ContentBlock {
    /// WI-47 Phase A→B: construct a tool call from a lossless value.
    pub fn tool_call(id: CallId, name: Option<String>, arguments: JsonValue) -> Self {
        ContentBlock::ToolCall {
            id,
            name,
            arguments,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Message {
    pub id: MessageId,
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
    pub source: MessageSource,
}

/// Provider-neutral classification for an auxiliary model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallPurpose {
    Compaction,
    SessionTitle,
}

/// Caller-declared tool-use policy for one model request, the neutral
/// projection of Anthropic's `tool_choice` / OpenAI's `tool_choice`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    /// The model decides whether to call a tool (the provider default).
    Auto,
    /// The model must call one of the provided tools.
    Any,
    /// The model must call the named tool.
    Tool { name: String },
}

/// One recorded usage sample captured from a model call's stream, the payload
/// of the `llm/usage-recorded` event that feeds the token meter.
#[derive(Debug, Clone)]
pub struct LlmUsageRecord {
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub purpose: Option<CallPurpose>,
    pub usage: TokenUsage,
    /// Unix epoch milliseconds at capture time.
    pub at_ms: u64,
}

#[derive(Debug, Clone)]
pub struct GenerateOptions {
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub messages: Vec<Message>,
    pub system: Option<String>,
    pub tools: Option<Vec<ToolSchema>>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub stop: Option<Vec<String>>,
    pub session_id: Option<String>,
    /// Agent attribution for metering/analytics; distinct from `session_id`
    /// when several agents share one session (subagents).
    pub agent_id: Option<String>,
    /// Caller-declared tool-use policy; `None` keeps the provider default
    /// (auto when tools are offered).
    pub tool_choice: Option<ToolChoice>,
    pub cancellation: Option<CancellationToken>,
    /// Auxiliary-call classification (compaction / session-title), matching
 /// `GenerateOptions.purpose`.
    pub purpose: Option<CallPurpose>,
    /// Process-local mark that this request was assembled by the agent loop,
 /// the Rust counterpart of `markAgentLoopRequest` identity WeakSet.
    pub loop_assembled: bool,
}

impl GenerateOptions {
    pub fn mark_agent_loop_request(&mut self) {
        self.loop_assembled = true;
    }

    pub fn is_agent_loop_request(&self) -> bool {
        self.loop_assembled
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

/// Serializable provider or transport failure facts; policy decides whether
/// they are retryable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmFailure {
    pub message: String,
    pub code: String,
    pub status: Option<u16>,
    pub provider_retry_after_ms: Option<u64>,
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    MaxTokens,
    Aborted {
        failure: LlmFailure,
    },
    Error {
        failure: LlmFailure,
    },
    Custom {
        kind: String,
        failure: Option<LlmFailure>,
    },
}

impl FinishReason {
    /// The byte stream ended without the provider's terminal marker
    /// (`[DONE]` / `message_stop` / a finish reason): the response was
    /// truncated mid-generation, never a clean stop. Surfaces as a
    /// retryable error instead of a silently empty completion (BUG-99).
    pub fn truncated_stream() -> FinishReason {
        FinishReason::Error {
            failure: LlmFailure {
                message: "provider closed the stream before a finish reason arrived".into(),
                code: "PROVIDER_STREAM_TRUNCATED".into(),
                status: None,
                provider_retry_after_ms: None,
                request_id: None,
            },
        }
    }
}

/// Adapter-private replay metadata riding the terminal `finish` chunk.
#[derive(Debug, Clone)]
pub struct ReplayEnvelope {
    /// Response-level adapter-private metadata.
    pub response: JsonValue,
    /// Per-block adapter-private metadata, one entry per emitted block.
    pub blocks: Option<Vec<JsonValue>>,
}

/// Raw streaming protocol emitted by adapters.
#[derive(Debug, Clone)]
pub enum StreamChunk {
    BlockStart {
        index: u64,
        block_type: ContentBlockType,
    },
    TextDelta {
        index: u64,
        text: String,
    },
    ReasoningDelta {
        index: u64,
        text: String,
    },
    ToolCallDelta {
        index: u64,
        id: CallId,
        name: Option<String>,
        arguments_delta: String,
    },
    BlockEnd {
        index: u64,
        block: ContentBlock,
    },
    Usage {
        usage: TokenUsage,
    },
    Finish {
        reason: FinishReason,
        replay_state: Option<ReplayEnvelope>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("no adapter for provider `{0}`")]
    NoAdapter(String),
    #[error("adapter for provider `{0}` is already registered")]
    DuplicateAdapter(String),
    #[error("invalid adapter: {0}")]
    InvalidAdapter(String),
    #[error("invalid configurable provider directory: {0}")]
    InvalidDirectory(String),
    #[error("model call failed: {code}: {message}")]
    Call {
        code: String,
        message: String,
        status: Option<u16>,
        provider_retry_after_ms: Option<u64>,
        request_id: Option<String>,
    },
    #[error("request cancelled")]
    Cancelled,
}

impl LlmError {
    pub fn code(&self) -> &str {
        match self {
            LlmError::NoAdapter(_) => "NO_ADAPTER",
            LlmError::DuplicateAdapter(_) => "DUPLICATE_ADAPTER",
            LlmError::InvalidAdapter(_) => "INVALID_ADAPTER",
            LlmError::InvalidDirectory(_) => "INVALID_DIRECTORY",
            LlmError::Call { code, .. } => code,
            LlmError::Cancelled => "ABORTED",
        }
    }

 /// Project the failure facts carried by this error, mirroring
    /// `LlmError.failure` snapshot.
    pub fn to_failure(&self) -> LlmFailure {
        let (status, provider_retry_after_ms, request_id) = match self {
            LlmError::Call {
                status,
                provider_retry_after_ms,
                request_id,
                ..
            } => (*status, *provider_retry_after_ms, request_id.clone()),
            _ => (None, None, None),
        };
        LlmFailure {
            message: self.to_string(),
            code: self.code().to_string(),
            status,
            provider_retry_after_ms,
            request_id,
        }
    }
}

/// Bounded exponential backoff with symmetric jitter around each local delay.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryBackoff {
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    pub jitter_ratio: f64,
}

impl Default for RetryBackoff {
    fn default() -> Self {
        Self {
            initial_delay_ms: 500,
            max_delay_ms: 10_000,
            jitter_ratio: 0.1,
        }
    }
}

/// Provider-owned model-request retry policy.
#[derive(Debug, Clone, PartialEq)]
pub enum RetryPolicy {
    Normal {
        max_retries: u32,
        retryable_codes: Vec<String>,
        backoff: RetryBackoff,
    },
    Always {
        backoff: RetryBackoff,
    },
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::Normal {
            max_retries: 5,
            // PROVIDER_STREAM_TRUNCATED (BUG-99's `truncated_stream`) is a
            // transport blip like any other — a re-ask is the recovery, and
            // leaving it out made every mid-stream disconnect terminal once
            // the loop started executing this policy (2026-10-04).
            retryable_codes: [
                "EMPTY_RESPONSE",
                "PROVIDER_STREAM_TRUNCATED",
                "RATE_LIMIT",
                "SERVER",
                "TIMEOUT",
                "TRANSPORT",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            backoff: RetryBackoff::default(),
        }
    }
}

impl RetryPolicy {
    pub fn backoff(&self) -> &RetryBackoff {
        match self {
            RetryPolicy::Normal { backoff, .. } | RetryPolicy::Always { backoff } => backoff,
        }
    }

    pub fn is_retryable(&self, code: &str) -> bool {
        match self {
            RetryPolicy::Always { .. } => true,
            RetryPolicy::Normal {
                retryable_codes, ..
            } => retryable_codes.iter().any(|candidate| candidate == code),
        }
    }
}

/// Validate, default, and detach one provider-owned retry policy, mirroring
/// `resolveRetryPolicy`. Omission selects the normal defaults.
pub fn resolve_retry_policy(
    config: Option<RetryPolicy>,
    path: &str,
) -> Result<RetryPolicy, LlmError> {
    let Some(config) = config else {
        return Ok(RetryPolicy::default());
    };
    validate_backoff(config.backoff(), path)?;
    if let RetryPolicy::Normal {
        max_retries,
        retryable_codes,
        ..
    } = &config
    {
        if *max_retries == u32::MAX {
            return Err(LlmError::InvalidAdapter(format!(
                "{path}.maxRetries must be a non-negative integer"
            )));
        }
        if retryable_codes.is_empty() {
            return Err(LlmError::InvalidAdapter(format!(
                "{path}.retryableCodes must not be empty"
            )));
        }
        if retryable_codes.iter().any(|code| code.is_empty()) {
            return Err(LlmError::InvalidAdapter(format!(
                "{path}.retryableCodes must contain only non-empty strings"
            )));
        }
        let mut seen = std::collections::HashSet::new();
        if retryable_codes.iter().any(|code| !seen.insert(code)) {
            return Err(LlmError::InvalidAdapter(format!(
                "{path}.retryableCodes must not contain duplicates"
            )));
        }
    }
    Ok(config)
}

fn validate_backoff(backoff: &RetryBackoff, path: &str) -> Result<(), LlmError> {
    if backoff.initial_delay_ms == 0
        || backoff.max_delay_ms == 0
        || backoff.initial_delay_ms > backoff.max_delay_ms
        || !(0.0..=1.0).contains(&backoff.jitter_ratio)
    {
        return Err(LlmError::InvalidAdapter(format!(
            "{path}.backoff must satisfy 0 < initialDelayMs <= maxDelayMs and 0 <= jitterRatio <= 1"
        )));
    }
    Ok(())
}

/// Provider-wire adapter for the harness message and stream vocabulary.
pub trait LlmAdapter: Send + Sync {
    fn providers(&self) -> &[String];

    fn stream(&self, options: GenerateOptions)
        -> BoxStream<'static, Result<StreamChunk, LlmError>>;

    /// Describe one provider route owned by this adapter.
    fn provider_info(&self, provider: &str) -> LlmProviderInfo {
        LlmProviderInfo {
            id: provider.into(),
            name: provider.into(),
        }
    }

    /// Provider-owned retry policy captured for one route.
    fn provider_retry_policy(&self, _provider: &str) -> Option<RetryPolicy> {
        None
    }

    /// Non-streaming convenience: fold the stream into plain text. Only
    /// canonical `BlockEnd` text blocks count — deltas are their preview.
    /// Callers needing reasoning/tools/usage should consume `stream` directly.
    fn chat(&self, options: GenerateOptions) -> BoxFuture<'static, Result<String, LlmError>> {
        let stream = self.stream(options);
        Box::pin(async move {
            let mut stream = stream;
            let mut text = String::new();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(StreamChunk::BlockEnd {
                        block: ContentBlock::Text(t),
                        ..
                    }) => text.push_str(&t),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Error { failure } | FinishReason::Aborted { failure },
                        ..
                    }) => {
                        return Err(LlmError::Call {
                            code: failure.code,
                            message: failure.message,
                            status: failure.status,
                            provider_retry_after_ms: failure.provider_retry_after_ms,
                            request_id: failure.request_id,
                        })
                    }
                    Err(error) => return Err(error),
                    _ => {}
                }
            }
            Ok(text)
        })
    }

    /// List models this adapter can currently advertise; advisory only.
    fn list_models(
        &self,
        _provider: &str,
    ) -> BoxFuture<'static, Result<Vec<LlmModelInfo>, LlmError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Resolve all metadata available for one exact model.
    fn resolve_model(
        &self,
        provider: &str,
        model: &str,
        _cancellation: Option<CancellationToken>,
    ) -> BoxFuture<'static, Result<LlmResolvedModelInfo, LlmError>> {
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
                default_max_tokens: None,
                reasoning: None,
            })
        })
    }
}

/// Run the `llm/stream` waterfall around the resolved adapter's stream and
/// flatten the shared output. Runtime-level and prepared-call dispatch share
/// this one assembly path so retry/replay/metrics wrappers always apply.
/// Every `Usage` chunk crossing this path is evented as `llm/usage-recorded`
/// for the token meter.
pub fn dispatch_llm_stream(
    bus: &EventBus,
    options: GenerateOptions,
    adapter: Arc<dyn LlmAdapter>,
) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
    let bus = bus.clone();
    let attribution = (
        options.session_id.clone(),
        options.agent_id.clone(),
        options.provider.clone(),
        options.model.clone(),
        options.purpose,
    );
    Box::pin(async_stream::stream! {
        let output = match bus
            .waterfall(
                Arc::new(LlmStreamRequest { options: options.clone() }),
                {
                    let adapter = Arc::clone(&adapter);
                    let options = options.clone();
                    move || {
                        let adapter = Arc::clone(&adapter);
                        let options = options.clone();
                        Box::pin(async move {
                            Ok(Arc::new(tokio::sync::Mutex::new(adapter.stream(options))))
                        })
                    }
                },
            )
            .await
        {
            Ok(output) => output,
            Err(error) => {
                yield Err(LlmError::Call {
                    code: "EVENT".into(),
                    message: error.to_string(),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                });
                return;
            }
        };

        // Flatten the shared stream; the guard is dropped between polls so the
        // returned stream stays Send. `Err` items are terminal failures, per
 // the Result-stream protocol (finish-chunk equivalent).
        loop {
            let item = { output.lock().await.next().await };
            match item {
                Some(chunk) => {
                    if let Ok(StreamChunk::Usage { usage }) = &chunk {
                        let (session_id, agent_id, provider, model, purpose) = &attribution;
                        bus.emit(Arc::new(LlmUsageRecorded {
                            record: LlmUsageRecord {
                                session_id: session_id.clone(),
                                agent_id: agent_id.clone(),
                                provider: provider.clone(),
                                model: model.clone(),
                                purpose: *purpose,
                                usage: *usage,
                                at_ms: now_epoch_ms(),
                            },
                        }));
                    }
                    yield chunk;
                }
                None => break,
            }
        }
    })
}

/// Wall-clock epoch milliseconds for usage attribution. Unix epoch keeps
/// window bucketing timezone-free and portable across processes.
pub fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Incremental chunk-to-message assembler, ported from `BlockAssembler`:
/// the canonical assembly algorithm the agent loop uses to build an assistant
/// message from a chunk stream while logging raw chunks for replay fidelity.
#[derive(Debug)]
struct PartialBlock {
    block_type: ContentBlockType,
    text: String,
    tool_call_id: Option<CallId>,
    tool_call_name: Option<String>,
    tool_call_arguments: String,
    block: Option<ContentBlock>,
}

/// WI-47 Phase B: the single text→JsonValue parse for tool-call arguments,
/// shared by the BlockAssembler (streaming ingress) and adapter wire
/// entries. Semantics locked here and only here: empty/whitespace → `{}`
/// malformed text → `JsonValue::String(raw)` (raw text
/// preserved as the argument — no silent Null), otherwise the lossless tree.
pub fn tool_call_arguments_from_text(text: &str) -> JsonValue {
    if text.trim().is_empty() {
        return JsonValue::Object(Vec::new());
    }
    JsonValue::from_json_str(text).unwrap_or_else(|_| JsonValue::String(text.to_string()))
}

/// Synthesize the raw-partial payload for an unclosed custom block: the
/// accumulated deltas, preserving as much content as possible.
fn raw_partial_payload(partial: &PartialBlock) -> JsonValue {
    let mut entries = Vec::new();
    if !partial.text.is_empty() {
        entries.push(("text".into(), JsonValue::String(partial.text.clone())));
    }
    if !partial.tool_call_arguments.is_empty() {
        entries.push((
            "arguments".into(),
            JsonValue::String(partial.tool_call_arguments.clone()),
        ));
    }
    JsonValue::Object(entries)
}

pub struct BlockAssembler {
    partials: HashMap<u64, PartialBlock>,
    order: Vec<u64>,
    usage: Option<TokenUsage>,
    finish: Option<FinishReason>,
    replay_state: Option<ReplayEnvelope>,
}

impl Default for BlockAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockAssembler {
    pub fn new() -> Self {
        Self {
            partials: HashMap::new(),
            order: Vec::new(),
            usage: None,
            finish: None,
            replay_state: None,
        }
    }

    fn ensure(&mut self, index: u64, block_type: ContentBlockType) -> &mut PartialBlock {
        let mut newly_inserted = false;
        let partial = self.partials.entry(index).or_insert_with(|| {
            newly_inserted = true;
            PartialBlock {
                block_type,
                text: String::new(),
                tool_call_id: None,
                tool_call_name: None,
                tool_call_arguments: String::new(),
                block: None,
            }
        });
        if newly_inserted {
            self.order.push(index);
        }
        partial
    }

    fn assemble_partial(&self, partial: &PartialBlock, index: u64) -> ContentBlock {
        if let Some(block) = &partial.block {
            return block.clone();
        }
        match &partial.block_type {
            ContentBlockType::Text => ContentBlock::Text(partial.text.clone()),
            ContentBlockType::Reasoning => ContentBlock::Reasoning(partial.text.clone()),
            ContentBlockType::ToolCall => ContentBlock::ToolCall {
                id: partial
                    .tool_call_id
                    .clone()
                    .unwrap_or_else(|| CallId(format!("call-{index}"))),
                name: partial.tool_call_name.clone(),
                // WI-47 Phase B: the ONE text→JsonValue parse on the
                // streaming ingress (delta accumulation stays textual).
                arguments: tool_call_arguments_from_text(&partial.tool_call_arguments),
            },
            ContentBlockType::Custom(kind) => ContentBlock::Custom {
                kind: kind.clone(),
                payload: raw_partial_payload(partial),
            },
            other => panic!("cannot assemble incomplete block of type {other:?}"),
        }
    }

    fn must_get(&self, index: u64) -> &PartialBlock {
        self.partials
            .get(&index)
            .expect("BlockAssembler invariant violated: no partial for index")
    }

    /// The one shared keep/drop decision over all seen blocks: max-token
    /// truncation drops tool calls that cannot be executed safely.
    fn assembled(&self) -> (Vec<ContentBlock>, Option<ReplayEnvelope>) {
        let all = self
            .order
            .iter()
            .map(|&index| self.assemble_partial(self.must_get(index), index))
            .collect::<Vec<_>>();
        let kept = match &self.finish {
            Some(FinishReason::MaxTokens) => Some(
                all.iter()
                    .map(|block| !matches!(block, ContentBlock::ToolCall { .. }))
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        let blocks = match &kept {
            Some(kept) => all
                .iter()
                .zip(kept)
                .filter(|(_, keep)| **keep)
                .map(|(block, _)| block.clone())
                .collect(),
            None => all.clone(),
        };
        let replay = match &self.replay_state {
            Some(envelope) => match &envelope.blocks {
                None => Some(envelope.clone()),
                Some(metadata) => {
                    if metadata.len() != all.len() {
                        return (blocks, None);
                    }
                    match kept {
                        Some(kept) if blocks.len() != all.len() => Some(ReplayEnvelope {
                            response: envelope.response.clone(),
                            blocks: Some(
                                metadata
                                    .iter()
                                    .zip(kept)
                                    .filter(|(_, keep)| *keep)
                                    .map(|(entry, _)| entry.clone())
                                    .collect(),
                            ),
                        }),
                        _ => Some(envelope.clone()),
                    }
                }
            },
            None => None,
        };
        (blocks, replay)
    }

    /// Feed one chunk into the assembly state.
    pub fn push(&mut self, chunk: StreamChunk) {
        match chunk {
            StreamChunk::BlockStart { index, block_type } => {
                self.ensure(index, block_type);
            }
            StreamChunk::TextDelta { index, text } => {
                let partial = self.ensure(index, ContentBlockType::Text);
                if partial.block.is_some() {
                    return;
                }
                partial.text.push_str(&text);
            }
            StreamChunk::ReasoningDelta { index, text } => {
                let partial = self.ensure(index, ContentBlockType::Reasoning);
                if partial.block.is_some() {
                    return;
                }
                partial.text.push_str(&text);
            }
            StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                let partial = self.ensure(index, ContentBlockType::ToolCall);
                if partial.block.is_some() {
                    return;
                }
                // BUG-42: adapters send argument-only deltas with an EMPTY
                // id (the "no id update" convention) — an unconditional
                // overwrite clobbered the real id recorded by the first
                // delta.
                if !id.0.is_empty() {
                    partial.tool_call_id = Some(id);
                }
                if let Some(name) = name {
                    partial.tool_call_name = Some(name);
                }
                partial.tool_call_arguments.push_str(&arguments_delta);
            }
            StreamChunk::BlockEnd { index, block } => {
                let block_type = match &block {
                    ContentBlock::Text(_) => ContentBlockType::Text,
                    ContentBlock::Reasoning(_) => ContentBlockType::Reasoning,
                    ContentBlock::Image { .. } => ContentBlockType::Image,
                    ContentBlock::ToolCall { .. } => ContentBlockType::ToolCall,
                    ContentBlock::ToolResult { .. } => ContentBlockType::ToolResult,
                    ContentBlock::Custom { kind, .. } => ContentBlockType::Custom(kind.clone()),
                };
                let partial = self.ensure(index, block_type);
                if partial.block.is_some() {
                    return;
                }
                partial.block = Some(block);
            }
            StreamChunk::Usage { usage } => self.usage = Some(usage),
            StreamChunk::Finish {
                reason,
                replay_state,
            } => {
                self.finish = Some(reason);
                self.replay_state = replay_state;
            }
        }
    }

    /// Assemble all blocks seen so far, in stream order. Max-token truncation
    /// drops tool calls; an open block assembles from its accumulated deltas
    /// (an uncloseable open block type panics, surfacing a protocol bug).
    pub fn blocks(&self) -> Vec<ContentBlock> {
        self.assembled().0
    }

    /// Assemble the prefix an interrupted stream can safely finalize: closed
    /// and open text/reasoning blocks with non-whitespace content, in stream
    /// order. Tool calls are omitted because interruption precedes dispatch.
    pub fn interrupted_blocks(&self) -> Vec<ContentBlock> {
        self.order
            .iter()
            .filter_map(|&index| {
                let partial = self.must_get(index);
                let block_type = match &partial.block {
                    Some(block) => match block {
                        ContentBlock::Text(_) => ContentBlockType::Text,
                        ContentBlock::Reasoning(_) => ContentBlockType::Reasoning,
                        ContentBlock::Custom { kind, .. } => ContentBlockType::Custom(kind.clone()),
                        _ => return None,
                    },
                    None => partial.block_type.clone(),
                };
                if let ContentBlockType::Custom(_) = &block_type {
                    return Some(self.assemble_partial(partial, index));
                }
                if block_type != ContentBlockType::Text && block_type != ContentBlockType::Reasoning
                {
                    return None;
                }
                match self.assemble_partial(partial, index) {
                    ContentBlock::Text(text) if text.trim().is_empty() => None,
                    ContentBlock::Reasoning(text) if text.trim().is_empty() => None,
                    block => Some(block),
                }
            })
            .collect()
    }

    pub fn usage(&self) -> Option<TokenUsage> {
        self.usage
    }

    /// Finish reason from the `finish` chunk; `Stop` when the stream ended
    /// without one.
    pub fn finish(&self) -> FinishReason {
        match self.finish.clone() {
            Some(reason) => reason,
            // BUG-99: the stream never delivered a terminal finish chunk —
            // a truncated response, never a clean Stop (defense in depth:
            // adapters that synthesize their own finish already carry the
            // truncation error from the translator layer).
            None => FinishReason::truncated_stream(),
        }
    }

    /// Replay metadata from the terminal finish chunk, with per-block entries
    /// pruned in step with [`BlockAssembler::blocks`].
    pub fn replay_state(&self) -> Option<ReplayEnvelope> {
        self.assembled().1
    }

    /// The assembled assistant message over [`BlockAssembler::blocks`].
    /// An omitted source defaults to the assembler plugin attribution,
 /// matching default `{ kind: 'plugin', plugin: 'starriver-kernel/assembler' }`.
    pub fn message(&self, source: Option<MessageSource>) -> Message {
        let source = source.unwrap_or_else(|| MessageSource::Plugin {
            plugin: "starriver-kernel/assembler".into(),
            form: None,
        });
        Message {
            id: mint_message_id(),
            role: MessageRole::Assistant,
            content: self.blocks(),
            source,
        }
    }
}

/// One model call whose config and adapter registration were resolved
/// together.
pub struct PreparedLlmCall {
    pub config: LlmCallConfig,
    pub retry_policy: RetryPolicy,
    pub adapter: Arc<dyn LlmAdapter>,
    pub adapter_defaults: LlmCallConfigAdapterDefaults,
    pub context: Option<LlmModelContext>,
    bus: EventBus,
    dispatched: AtomicBool,
}

impl PreparedLlmCall {
    /// Build a prepared call from a resolved config, its retry policy, and the
    /// owning adapter.
    pub fn new(
        config: LlmCallConfig,
        retry_policy: RetryPolicy,
        adapter: Arc<dyn LlmAdapter>,
        bus: EventBus,
    ) -> Self {
        Self {
            config,
            retry_policy,
            adapter,
            adapter_defaults: LlmCallConfigAdapterDefaults::default(),
            context: None,
            bus,
            dispatched: AtomicBool::new(false),
        }
    }

    /// Attach exact-model resolution facts (adapter-materialized defaults and
    /// context capacity) captured at preparation time.
    pub fn with_resolution(
        mut self,
        adapter_defaults: LlmCallConfigAdapterDefaults,
        context: Option<LlmModelContext>,
    ) -> Self {
        self.adapter_defaults = adapter_defaults;
        self.context = context;
        self
    }

    pub fn stream(
        &self,
        options: GenerateOptions,
    ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
        if self.dispatched.swap(true, Ordering::SeqCst) {
            return Box::pin(futures::stream::once(async {
                Err(LlmError::Call {
                    code: "INVALID_PREPARED_CALL".into(),
                    message: "prepared LLM call can only be dispatched once".into(),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                })
            }));
        }
        if !generate_options_matches_config(&options, &self.config) {
            return Box::pin(futures::stream::once(async {
                Err(LlmError::Call {
                    code: "INVALID_PREPARED_CALL".into(),
                    message: "prepared LLM call config changed before adapter dispatch".into(),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                })
            }));
        }
        dispatch_llm_stream(&self.bus, options, Arc::clone(&self.adapter))
    }
}

/// Product seam for provider routing, capability resolution, and streaming.
pub trait LlmRuntime: Service {
    const NAME: &'static str = "llm";

    fn register_adapter(&self, adapter: Arc<dyn LlmAdapter>) -> Result<EffectHandle, LlmError>;

    /// Atomically replace one registration's route set with a new adapter.
    /// The new routes commit before the old ones are removed, and the
    /// topology change is announced once.
    fn replace_adapter(
        &self,
        providers: Vec<String>,
        adapter: Arc<dyn LlmAdapter>,
    ) -> Result<EffectHandle, LlmError>;

    fn list_providers(&self) -> Vec<LlmProviderInfo>;

    fn provider_retry_policy(&self, provider: &str) -> Result<RetryPolicy, LlmError>;

    fn list_models(
        &self,
        provider: &str,
    ) -> BoxFuture<'static, Result<Vec<LlmModelInfo>, LlmError>>;

    fn resolve_model_info(
        &self,
        provider: &str,
        model: &str,
        cancellation: Option<CancellationToken>,
    ) -> BoxFuture<'static, Result<LlmResolvedModelInfo, LlmError>>;

    fn resolve_call_config(
        &self,
        config: LlmCallConfig,
        cancellation: Option<CancellationToken>,
    ) -> BoxFuture<'static, Result<LlmCallConfig, LlmError>>;

    fn prepare_call(
        &self,
        config: LlmCallConfig,
        cancellation: Option<CancellationToken>,
    ) -> BoxFuture<'static, Result<PreparedLlmCall, LlmError>>;

    fn stream(&self, options: GenerateOptions)
        -> BoxStream<'static, Result<StreamChunk, LlmError>>;

    /// Non-streaming convenience: fold the provider's stream into plain text
 /// (title-LLM / memory llm_select / compaction), mirroring
    /// `ctx.llm.chat`. Only canonical `BlockEnd` text blocks count.
    fn chat(&self, options: GenerateOptions) -> BoxFuture<'static, Result<String, LlmError>>;

    fn register_configurable_providers(
        &self,
        entries: Vec<LlmConfigurableProvider>,
    ) -> Result<EffectHandle, LlmError>;

    fn list_configurable_providers(&self) -> Vec<LlmConfigurableProvider>;

    fn register_model_discovery(
        &self,
        settings_ns: String,
        discover: LlmModelDiscovery,
    ) -> Result<EffectHandle, LlmError>;

    fn discover_models(
        &self,
        settings_ns: String,
        request: LlmModelDiscoveryRequest,
    ) -> BoxFuture<'static, Result<Vec<LlmDiscoveredModel>, LlmError>>;
}

pub type LlmStreamOutput =
    Arc<tokio::sync::Mutex<BoxStream<'static, Result<StreamChunk, LlmError>>>>;

cordis_rslite::cordis_event!(LlmStreamRequest, "llm/stream", {
    options: GenerateOptions,
});

impl WaterfallEvent for LlmStreamRequest {
    type Output = LlmStreamOutput;
}

cordis_rslite::cordis_event!(LlmAdaptersUpdated, "llm/adapters-updated", {
    providers: Vec<String>,
});

cordis_rslite::cordis_event!(LlmUsageRecorded, "llm/usage-recorded", {
    record: LlmUsageRecord,
});

#[cfg(test)]
mod tests {
    use super::*;

    /// WI-47 Phase B: ingress parse (tool_call_arguments_from_text) is the
    /// single text→JsonValue point; lossless samples (u64 max, deep nesting,
    /// non-ASCII keys) survive byte-equivalent.
    #[test]
    fn tool_call_arguments_json_lossless_round_trip() {
        let raw = r#"{"路径":"配置","big":18446744073709551615,"neg":-17,"nested":{"deep":[1,true,null]}}"#;
        let value = tool_call_arguments_from_text(raw);
        let call = ContentBlock::tool_call(CallId("c1".into()), Some("bash".into()), value.clone());
        assert_eq!(value.to_json_string().unwrap(), raw);
        assert_eq!(
            value.get("nested").and_then(|v| v.get("deep")),
            Some(&JsonValue::Array(vec![
                JsonValue::Int(1),
                JsonValue::Bool(true),
                JsonValue::Null,
            ]))
        );
    }

    #[test]
    fn tool_call_arguments_json_malformed_degrades_to_raw_string() {
        // The ingress keeps malformed model text as a String argument —
        // no silent Null anywhere on the path.
        assert_eq!(
            tool_call_arguments_from_text("{bad json"),
            JsonValue::String("{bad json".into())
        );
        let call = ContentBlock::tool_call(
            CallId("c1".into()),
            Some("bash".into()),
            tool_call_arguments_from_text("{bad json"),
        );
        let ContentBlock::ToolCall { arguments, .. } = call else {
            panic!("constructor must build a ToolCall");
        };
        assert_eq!(arguments, JsonValue::String("{bad json".into()));
    }

    #[test]
    fn tool_call_arguments_json_empty_maps_to_empty_object() {
        assert_eq!(
            tool_call_arguments_from_text(""),
            JsonValue::Object(Vec::new())
        );
        assert_eq!(
            tool_call_arguments_from_text("   "),
            JsonValue::Object(Vec::new())
        );
    }

    #[test]
    fn tool_call_constructor_serializes_once_and_round_trips() {
        let value = JsonValue::Object(vec![(
            "cmd".into(),
            JsonValue::Array(vec![JsonValue::String("ls -la".into())]),
        )]);
        let call = ContentBlock::tool_call(CallId("c1".into()), Some("bash".into()), value.clone());
        let ContentBlock::ToolCall { arguments, .. } = call else {
            panic!("constructor must build a ToolCall");
        };
        assert_eq!(arguments, value);
    }

    fn text_start(index: u64) -> StreamChunk {
        StreamChunk::BlockStart {
            index,
            block_type: ContentBlockType::Text,
        }
    }

    fn text_delta(index: u64, text: &str) -> StreamChunk {
        StreamChunk::TextDelta {
            index,
            text: text.into(),
        }
    }

    #[test]
    fn custom_variants_construct_and_match() {
        let block = ContentBlock::Custom {
            kind: "video".into(),
            payload: JsonValue::Object(vec![("url".into(), JsonValue::String("https://x".into()))]),
        };
        assert!(matches!(
            block,
            ContentBlock::Custom { kind, .. } if kind == "video"
        ));
        let source = MessageSource::Custom {
            kind: "sensor".into(),
            payload: JsonValue::Null,
        };
        assert!(matches!(
            source,
            MessageSource::Custom { kind, .. } if kind == "sensor"
        ));
        let finish = FinishReason::Custom {
            kind: "video-failed".into(),
            failure: None,
        };
        assert!(matches!(
            finish,
            FinishReason::Custom { kind, .. } if kind == "video-failed"
        ));
    }

    #[test]
    fn assembler_passes_custom_block_through_block_end() {
        let mut assembler = BlockAssembler::new();
        assembler.push(StreamChunk::BlockStart {
            index: 0,
            block_type: ContentBlockType::Custom("video".into()),
        });
        assembler.push(StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Custom {
                kind: "video".into(),
                payload: JsonValue::Object(vec![(
                    "url".into(),
                    JsonValue::String("https://x".into()),
                )]),
            },
        });
        let blocks = assembler.blocks();
        assert!(matches!(
            blocks.as_slice(),
            [ContentBlock::Custom { kind, payload }]
            if kind == "video" && payload.get("url").and_then(JsonValue::as_str) == Some("https://x")
        ));
    }

    #[test]
    fn assembler_keeps_unclosed_custom_block_as_raw_partial() {
        let mut assembler = BlockAssembler::new();
        assembler.push(StreamChunk::BlockStart {
            index: 0,
            block_type: ContentBlockType::Custom("video".into()),
        });
        assembler.push(StreamChunk::TextDelta {
            index: 0,
            text: "draft".into(),
        });
        let blocks = assembler.blocks();
        assert!(matches!(
            blocks.as_slice(),
            [ContentBlock::Custom { kind, payload }]
            if kind == "video" && payload.get("text").and_then(JsonValue::as_str) == Some("draft")
        ));
        // Interrupted prefix keeps the custom partial too.
        let interrupted = assembler.interrupted_blocks();
        assert!(matches!(
            interrupted.as_slice(),
            [ContentBlock::Custom { kind, .. }] if kind == "video"
        ));
    }

    /// BUG-42 regression: adapters send argument-only ToolCallDelta chunks
    /// with an EMPTY id ("no id update" convention, see the ollama/openai
    /// translators). The assembler overwrote the already-recorded real call
    /// id with that empty id, so an interrupted stream assembled a ToolCall
    /// with CallId("") instead of the id from the first delta.
    #[test]
    fn assembler_empty_tool_call_delta_keeps_the_recorded_id() {
        let mut assembler = BlockAssembler::new();
        assembler.push(StreamChunk::BlockStart {
            index: 1,
            block_type: ContentBlockType::ToolCall,
        });
        assembler.push(StreamChunk::ToolCallDelta {
            index: 1,
            id: CallId("call_1".into()),
            name: Some("bash".into()),
            arguments_delta: String::new(),
        });
        assembler.push(StreamChunk::ToolCallDelta {
            index: 1,
            id: CallId(String::new()),
            name: None,
            arguments_delta: "{\"a\":".into(),
        });
        let blocks = assembler.blocks();
        match blocks.as_slice() {
            [ContentBlock::ToolCall { id, name, arguments }] => {
                assert_eq!(id.0, "call_1", "empty-id deltas must not clobber the id");
                assert_eq!(name.as_deref(), Some("bash"));
                // Unclosed partial keeps the accumulated argument text.
                assert_eq!(
                    arguments.as_str().map(str::to_string),
                    Some("{\"a\":".to_string())
                );
            }
            other => panic!("expected one ToolCall block, got {other:?}"),
        }
    }

    #[test]
    fn max_tokens_truncation_keeps_custom_blocks() {
        let mut assembler = BlockAssembler::new();
        assembler.push(StreamChunk::BlockStart {
            index: 0,
            block_type: ContentBlockType::Custom("video".into()),
        });
        assembler.push(StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Custom {
                kind: "video".into(),
                payload: JsonValue::Object(vec![(
                    "url".into(),
                    JsonValue::String("https://x".into()),
                )]),
            },
        });
        assembler.push(StreamChunk::Finish {
            reason: FinishReason::MaxTokens,
            replay_state: None,
        });
        let blocks = assembler.blocks();
        assert!(matches!(
            blocks.as_slice(),
            [ContentBlock::Custom { kind, .. }] if kind == "video"
        ));
    }

    #[test]
    fn assembler_builds_text_message() {
        let mut assembler = BlockAssembler::new();
        assembler.push(text_start(0));
        assembler.push(text_delta(0, "hel"));
        assembler.push(text_delta(0, "lo"));
        assembler.push(StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Text("hello".into()),
        });
        assembler.push(StreamChunk::Finish {
            reason: FinishReason::Stop,
            replay_state: None,
        });

        let blocks = assembler.blocks();
        assert!(matches!(blocks.as_slice(), [ContentBlock::Text(text)] if text == "hello"));
        assert_eq!(assembler.finish(), FinishReason::Stop);
        assert!(assembler.replay_state().is_none());

        let message = assembler.message(Some(MessageSource::Model {
            provider: "fake".into(),
            model: "m".into(),
            replay_state: None,
        }));
        assert_eq!(message.role, MessageRole::Assistant);
        assert_eq!(message.content.len(), 1);

        let defaulted = assembler.message(None);
        assert!(matches!(
            defaulted.source,
            MessageSource::Plugin { plugin, .. } if plugin == "starriver-kernel/assembler"
        ));
    }

    #[test]
    fn assembler_tolerates_delta_only_protocol() {
        let mut assembler = BlockAssembler::new();
        // No block-start; the delta opens the partial implicitly.
        assembler.push(text_delta(0, "delta-only"));
        let blocks = assembler.blocks();
        assert!(matches!(blocks.as_slice(), [ContentBlock::Text(text)] if text == "delta-only"));
    }

    #[test]
    fn assembler_ignores_stragglers_after_block_end() {
        let mut assembler = BlockAssembler::new();
        assembler.push(text_start(0));
        assembler.push(StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Text("closed".into()),
        });
        assembler.push(text_delta(0, "-straggler"));
        let blocks = assembler.blocks();
        assert!(matches!(blocks.as_slice(), [ContentBlock::Text(text)] if text == "closed"));
    }

    #[test]
    fn assembler_builds_tool_call_from_deltas() {
        let mut assembler = BlockAssembler::new();
        assembler.push(StreamChunk::BlockStart {
            index: 1,
            block_type: ContentBlockType::ToolCall,
        });
        assembler.push(StreamChunk::ToolCallDelta {
            index: 1,
            id: CallId("c1".into()),
            name: Some("bash".into()),
            arguments_delta: "{\"cmd\":".into(),
        });
        assembler.push(StreamChunk::ToolCallDelta {
            index: 1,
            id: CallId("c1".into()),
            name: None,
            arguments_delta: "\"ls\"}".into(),
        });
        assembler.push(StreamChunk::BlockEnd {
            index: 1,
            block: ContentBlock::ToolCall {
                id: CallId("c1".into()),
                name: Some("bash".into()),
                arguments: JsonValue::Object(vec![("cmd".into(), JsonValue::String("ls".into()))]),
            },
        });
        let blocks = assembler.blocks();
        assert!(matches!(
            blocks.as_slice(),
            [ContentBlock::ToolCall { id, name, arguments }]
            if id.0 == "c1"
                && name.as_deref() == Some("bash")
                && arguments == &JsonValue::Object(vec![(
                    "cmd".into(),
                    JsonValue::String("ls".into())
                )])
        ));
    }

    #[test]
    fn max_tokens_truncation_drops_tool_calls_and_prunes_replay() {
        let mut assembler = BlockAssembler::new();
        assembler.push(text_start(0));
        assembler.push(text_delta(0, "text"));
        assembler.push(StreamChunk::BlockStart {
            index: 1,
            block_type: ContentBlockType::ToolCall,
        });
        assembler.push(StreamChunk::ToolCallDelta {
            index: 1,
            id: CallId("c1".into()),
            name: None,
            arguments_delta: "{}".into(),
        });
        assembler.push(StreamChunk::Finish {
            reason: FinishReason::MaxTokens,
            replay_state: Some(ReplayEnvelope {
                response: JsonValue::String("resp".into()),
                blocks: Some(vec![
                    JsonValue::String("b0".into()),
                    JsonValue::String("b1".into()),
                ]),
            }),
        });
        let blocks = assembler.blocks();
        assert!(matches!(blocks.as_slice(), [ContentBlock::Text(text)] if text == "text"));
        let replay = assembler.replay_state().unwrap();
        assert_eq!(replay.blocks.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn interrupted_blocks_keep_text_and_drop_tool_calls() {
        let mut assembler = BlockAssembler::new();
        assembler.push(text_start(0));
        assembler.push(text_delta(0, "partial text"));
        assembler.push(StreamChunk::BlockStart {
            index: 1,
            block_type: ContentBlockType::ToolCall,
        });
        assembler.push(StreamChunk::ToolCallDelta {
            index: 1,
            id: CallId("c1".into()),
            name: None,
            arguments_delta: "{}".into(),
        });
        let interrupted = assembler.interrupted_blocks();
        assert!(matches!(
            interrupted.as_slice(),
            [ContentBlock::Text(text)] if text == "partial text"
        ));
    }

    #[test]
    fn assembler_tracks_usage() {
        let mut assembler = BlockAssembler::new();
        assembler.push(StreamChunk::Usage {
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: Some(2),
                cache_write_tokens: None,
                reasoning_tokens: Some(1),
            },
        });
        let usage = assembler.usage().unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.cache_read_tokens, Some(2));
        assert_eq!(usage.reasoning_tokens, Some(1));
    }

    #[test]
    fn loop_request_mark_and_failure_projection() {
        let mut options = GenerateOptions {
            provider: "fake".into(),
            model: "m".into(),
            reasoning_effort: None,
            messages: Vec::new(),
            system: None,
            tools: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            session_id: None,
            agent_id: None,
            tool_choice: None,
            cancellation: None,
            purpose: Some(CallPurpose::Compaction),
            loop_assembled: false,
        };
        assert!(!options.is_agent_loop_request());
        options.mark_agent_loop_request();
        assert!(options.is_agent_loop_request());
        assert_eq!(options.purpose, Some(CallPurpose::Compaction));
        assert_eq!(options.tool_choice, None);

        let error = LlmError::Call {
            code: "RATE_LIMIT".into(),
            message: "slow down".into(),
            status: Some(429),
            provider_retry_after_ms: Some(1000),
            request_id: Some("r1".into()),
        };
        let failure = error.to_failure();
        assert_eq!(failure.code, "RATE_LIMIT");
        assert_eq!(failure.status, Some(429));
        assert_eq!(failure.provider_retry_after_ms, Some(1000));
    }

    #[test]
    fn retry_policy_defaults_and_retryable_codes() {
        let policy = RetryPolicy::default();
        assert!(policy.is_retryable("RATE_LIMIT"));
        assert!(!policy.is_retryable("AUTH"));
        // 2026-10-04: BUG-99's truncated-stream code must be retryable — a
        // mid-stream transport disconnect is transient like any other.
        assert!(policy.is_retryable("PROVIDER_STREAM_TRUNCATED"));
        match &policy {
            RetryPolicy::Normal {
                max_retries,
                backoff,
                ..
            } => {
                assert_eq!(*max_retries, 5);
                assert_eq!(backoff.initial_delay_ms, 500);
                assert_eq!(backoff.max_delay_ms, 10_000);
            }
            RetryPolicy::Always { .. } => panic!("expected normal default"),
        }
        let always = RetryPolicy::Always {
            backoff: RetryBackoff::default(),
        };
        assert!(always.is_retryable("anything"));
    }

    #[tokio::test]
    async fn dispatch_llm_stream_events_usage_with_attribution() {
        use std::sync::Mutex as StdMutex;

        struct MeteredAdapter;
        impl LlmAdapter for MeteredAdapter {
            fn providers(&self) -> &[String] {
                static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
                PROVIDERS.get_or_init(|| vec!["metered".into()])
            }

            fn stream(
                &self,
                _options: GenerateOptions,
            ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
                Box::pin(async_stream::stream! {
                    yield Ok(StreamChunk::Usage {
                        usage: TokenUsage {
                            input_tokens: 7,
                            output_tokens: 3,
                            cache_read_tokens: Some(2),
                            cache_write_tokens: Some(1),
                            reasoning_tokens: None,
                        },
                    });
                    yield Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    });
                })
            }
        }

        let bus = EventBus::new();
        let seen = std::sync::Arc::new(StdMutex::new(Vec::new()));
        let _listener = bus.on_sync::<LlmUsageRecorded, _>({
            let seen = Arc::clone(&seen);
            move |event| {
                seen.lock().unwrap().push(event.record.clone());
                Ok(cordis_rslite::Value::new(()))
            }
        });
        let options = GenerateOptions {
            provider: "metered".into(),
            model: "m".into(),
            reasoning_effort: None,
            messages: Vec::new(),
            system: None,
            tools: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            session_id: Some("s1".into()),
            agent_id: Some("a1".into()),
            tool_choice: None,
            cancellation: None,
            purpose: Some(CallPurpose::SessionTitle),
            loop_assembled: false,
        };
        let chunks: Vec<_> = dispatch_llm_stream(&bus, options, Arc::new(MeteredAdapter))
            .collect()
            .await;
        assert_eq!(chunks.len(), 2);
        let records = seen.lock().unwrap().clone();
        assert_eq!(records.len(), 1, "one usage chunk must emit one record");
        let record = &records[0];
        assert_eq!(record.session_id.as_deref(), Some("s1"));
        assert_eq!(record.agent_id.as_deref(), Some("a1"));
        assert_eq!(record.provider, "metered");
        assert_eq!(record.model, "m");
        assert_eq!(record.purpose, Some(CallPurpose::SessionTitle));
        assert_eq!(record.usage.input_tokens, 7);
        assert_eq!(record.usage.cache_write_tokens, Some(1));
        assert!(record.at_ms > 0);
    }
}
