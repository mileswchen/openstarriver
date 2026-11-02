//! Anthropic wire: request serialization + SSE frame parser + translator.
//! User-turn image blocks project to base64 image sources (attachment
//! admission → inline data-URL projection).

use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::{
    CallId, ContentBlock, ContentBlockType, FinishReason, GenerateOptions, LlmError, MessageRole,
    ReplayEnvelope, StreamChunk, TokenUsage, ToolChoice,
};

/// Kernel `Custom` block kind carrying the raw Anthropic thinking block
/// (`thinking` text + `signature`), so the block survives the session and is
/// replayed verbatim on the next request while extended thinking is enabled.
/// Anthropic rejects a replayed thinking block whose signature is missing or
/// altered.
pub const THINKING_BLOCK_KIND: &str = "anthropic.thinking_block";

/// Output cap when the caller leaves `max_tokens` unset — the Messages API
/// requires the field (BUG-168: named now that the thinking budget
/// interacts with it).
pub const DEFAULT_MAX_TOKENS: u64 = 4096;

/// Headroom kept above the thinking budget when the cap has to be raised:
/// the Messages API requires `max_tokens > thinking.budget_tokens`.
const THINKING_MARGIN_TOKENS: u64 = 2048;

/// Map the caller's reasoning effort to an extended-thinking budget
/// (BUG-163): Claude extended thinking must be requested explicitly, and the
/// capture/replay seam below already round-trips thinking blocks — but
/// nothing ever ENABLED them, so a configured effort silently no-opped.
/// `None`/`"off"` leave thinking to the provider default (zhipu-style
/// anthropic-compat relays think by default and still round-trip fine).
fn thinking_budget(effort: Option<&str>) -> Result<Option<u64>, LlmError> {
    let invalid = |value: &str| LlmError::Call {
        code: "UNSUPPORTED_REASONING_EFFORT".into(),
        message: format!("Anthropic route does not support reasoning effort \"{value}\""),
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    };
    match effort {
        None | Some("off") | Some("") => Ok(None),
        Some("low") => Ok(Some(2_048)),
        Some("high") => Ok(Some(8_192)),
        Some("max") => Ok(Some(16_384)),
        Some(other) => Err(invalid(other)),
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::new()
}

pub fn serialize_request(options: &GenerateOptions) -> Result<serde_json::Value, LlmError> {
    // System contributions: the caller's `system` string plus any System-role
    // messages, each as its own text block (Anthropic accepts a string for a
    // single segment and a text-block array for several; System-role messages
    // must not leak into `messages`, which only allows user/assistant).
    let mut system_segments: Vec<String> = Vec::new();
    if let Some(system) = options.system.as_deref() {
        if !system.is_empty() {
            system_segments.push(system.to_string());
        }
    }
    let mut messages = Vec::new();
    for message in &options.messages {
        match message.role {
            MessageRole::System => {
                let text = text_of(&message.content);
                if !text.is_empty() {
                    system_segments.push(text);
                }
            }
            MessageRole::User => {
                if let Some(content) = user_content(&message.content)? {
                    messages.push(serde_json::json!({"role": "user", "content": content}));
                }
            }
            MessageRole::Assistant => {
                if let Some(content) = assistant_content(&message.content) {
                    messages.push(serde_json::json!({"role": "assistant", "content": content}));
                }
            }
        }
    }
    // BUG-170: the Messages API requires the FIRST message to carry the
    // user role, and strict anthropic-compat gateways enforce full role
    // alternation (the official endpoint folds consecutive same-role turns
    // server-side; a replayed/imported session can still start on an
    // assistant turn or carry adjacent same-role turns). Normalize here.
    let messages = merge_consecutive_roles(messages);
    let messages = if matches!(
        messages
            .first()
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str()),
        Some("assistant")
    ) {
        let mut seeded = vec![serde_json::json!({
            "role": "user",
            "content": "(session start)",
        })];
        seeded.extend(messages);
        seeded
    } else {
        messages
    };
    let mut tools = Vec::new();
    for schema in options.tools.iter().flatten() {
        tools.push(serde_json::json!({
            "name": schema.name,
            "description": schema.description,
            "input_schema": json_to_serde(&schema.parameters),
        }));
    }
    // BUG-163: an explicit effort enables extended thinking with a budget
    // ladder; `max_tokens` must strictly exceed the budget, so a caller cap
    // at/below it is raised with headroom.
    let mut max_tokens = options.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let thinking = thinking_budget(options.reasoning_effort.as_deref())?;
    if let Some(budget) = thinking {
        if max_tokens <= budget {
            max_tokens = budget + THINKING_MARGIN_TOKENS;
        }
    }
    let mut body = serde_json::json!({
        "model": options.model,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
    });
    if let Some(budget) = thinking {
        body["thinking"] = serde_json::json!({"type": "enabled", "budget_tokens": budget});
    }
    match system_segments.len() {
        0 => {}
        1 => {
            body["system"] = serde_json::json!(system_segments.remove(0));
        }
        _ => {
            body["system"] = serde_json::Value::Array(
                system_segments
                    .into_iter()
                    .map(|text| serde_json::json!({"type": "text", "text": text}))
                    .collect(),
            );
        }
    }
    if !tools.is_empty() {
        body["tools"] = serde_json::json!(tools);
        // Tool-use policy from the caller; omitted-but-tooled means `auto`,
        // made explicit so the request body always declares its intent.
        body["tool_choice"] = match options.tool_choice.as_ref() {
            None | Some(ToolChoice::Auto) => serde_json::json!({"type": "auto"}),
            Some(ToolChoice::Any) => serde_json::json!({"type": "any"}),
            Some(ToolChoice::Tool { name }) => serde_json::json!({"type": "tool", "name": name}),
        };
    }
    // BUG-41: caller parameters ride the request — the Messages API natively
    // supports `temperature` and `stop_sequences`; dropping them froze every
    // anthropic route at the provider default.
    if let Some(temperature) = options.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    if let Some(stop) = options.stop.as_ref().filter(|s| !s.is_empty()) {
        body["stop_sequences"] = serde_json::json!(stop);
    }
    Ok(body)
}

/// Serialize one user turn: plain text stays a string (wire compat), while
/// turns carrying tool results or images (or mixed blocks) become a block
/// array. Image blocks project to Anthropic base64 image sources.
fn user_content(blocks: &[ContentBlock]) -> Result<Option<serde_json::Value>, LlmError> {
    let text = text_of(blocks);
    let has_block_shapes = blocks.iter().any(|block| {
        matches!(
            block,
            ContentBlock::ToolResult { .. } | ContentBlock::Image { .. }
        )
    });
    if !has_block_shapes {
        if text.is_empty() {
            return Ok(None);
        }
        return Ok(Some(serde_json::json!(text)));
    }
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => {
                if !t.is_empty() {
                    parts.push(serde_json::json!({"type": "text", "text": t}));
                }
            }
            ContentBlock::Image { attachment } => {
                let (media_type, data) = base64_image_source(attachment)?;
                parts.push(serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": data,
                    },
                }));
            }
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                let output = text_of(content);
                // BUG-172: tool results may carry image blocks
                // (screenshot-class tools); tool_result accepts a content
                // array — text-only output stays a plain string for wire
                // compatibility.
                let images: Vec<&JsonValue> = content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Image { attachment } => Some(attachment),
                        _ => None,
                    })
                    .collect();
                let mut part = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": call_id.0.clone(),
                });
                if !images.is_empty() {
                    let mut tool_parts = Vec::new();
                    if !output.is_empty() {
                        tool_parts.push(serde_json::json!({"type": "text", "text": output}));
                    }
                    for attachment in images {
                        let (media_type, data) = base64_image_source(attachment)?;
                        tool_parts.push(serde_json::json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": data,
                            },
                        }));
                    }
                    part["content"] = serde_json::Value::Array(tool_parts);
                } else if !output.is_empty() {
                    part["content"] = serde_json::json!(output);
                }
                if *is_error {
                    part["is_error"] = serde_json::json!(true);
                }
                parts.push(part);
            }
            _ => {}
        }
    }
    Ok(Some(serde_json::Value::Array(parts)))
}

/// BUG-170: fold adjacent same-role wire turns into one (content arrays are
/// concatenated; a plain string content becomes a text block). Purely
/// additive normalization — the official API merges these server-side
/// anyway, and strict alternation gateways require it.
fn merge_consecutive_roles(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let to_blocks = |content: serde_json::Value| -> Vec<serde_json::Value> {
        match content {
            serde_json::Value::Array(parts) => parts,
            serde_json::Value::String(text) => {
                vec![serde_json::json!({"type": "text", "text": text})]
            }
            _ => Vec::new(),
        }
    };
    let mut merged: Vec<serde_json::Value> = Vec::new();
    for message in messages {
        let role = message.get("role").cloned();
        let adjacent = merged
            .last_mut()
            .filter(|last| last.get("role").cloned() == role);
        match adjacent {
            Some(last) => {
                let mut parts = to_blocks(last.get("content").cloned().unwrap_or_default());
                parts.extend(to_blocks(
                    message.get("content").cloned().unwrap_or_default(),
                ));
                last["content"] = serde_json::Value::Array(parts);
            }
            None => merged.push(message),
        }
    }
    merged
}

/// Split one Image block's attachment into `(media_type, base64)`. The
/// attachment seam projects hot images to inline data URLs, so anything else
/// (missing url, http ref) cannot ride the base64 source and fails loudly
/// rather than silently dropping the image (`INVALID_ATTACHMENT_REF`,
/// openai-adapter parity).
fn base64_image_source(attachment: &JsonValue) -> Result<(String, String), LlmError> {
    let invalid = |message: String| LlmError::Call {
        code: "INVALID_ATTACHMENT_REF".into(),
        message,
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    };
    let url = match attachment {
        JsonValue::Object(entries) => entries
            .iter()
            .find(|(key, _)| key == "url")
            .and_then(|(_, value)| value.as_str())
            .map(str::to_string),
        _ => None,
    };
    let Some(url) = url.filter(|url| !url.is_empty()) else {
        return Err(invalid(
            "image block attachment must carry a non-empty `url`".into(),
        ));
    };
    let rest = url.strip_prefix("data:").ok_or_else(|| {
        invalid(
            "image attachment url must be an inline `data:` URL; an http ref cannot ride the base64 source"
                .into(),
        )
    })?;
    let (media_type, data) = rest.split_once(";base64,").ok_or_else(|| {
        invalid("image data URL must be `data:<media_type>;base64,<payload>`".into())
    })?;
    if media_type.is_empty() || data.is_empty() {
        return Err(invalid(
            "image data URL must carry both media_type and payload".into(),
        ));
    }
    Ok((media_type.to_string(), data.to_string()))
}

/// Serialize one assistant turn as a block array: text, tool_use, and — while
/// extended thinking is enabled — the thinking blocks captured verbatim from
/// the prior response (signature included). Turns with only text stay strings.
fn assistant_content(blocks: &[ContentBlock]) -> Option<serde_json::Value> {
    let has_block_shapes = blocks.iter().any(|block| {
        matches!(
            block,
            ContentBlock::ToolCall { .. } | ContentBlock::Custom { .. }
        )
    });
    if !has_block_shapes {
        let text = text_of(blocks);
        if text.is_empty() {
            return None;
        }
        return Some(serde_json::json!(text));
    }
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => {
                if !t.is_empty() {
                    parts.push(serde_json::json!({"type": "text", "text": t}));
                }
            }
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => {
                // WI-47 Phase B: one lossless serialization at this format
                // boundary (was: String → parse → serialize every request).
                let input: serde_json::Value = arguments
                    .to_serde_json()
                    .unwrap_or_else(|_| serde_json::json!({}));
                parts.push(serde_json::json!({
                    "type": "tool_use",
                    "id": id.0.clone(),
                    "name": name.clone().unwrap_or_default(),
                    "input": input,
                }));
            }
            ContentBlock::Custom { kind, payload } if kind == THINKING_BLOCK_KIND => {
                // Replay the captured thinking block verbatim: Anthropic
                // validates the signature across turns.
                parts.push(json_to_serde(payload));
            }
            _ => {}
        }
    }
    Some(serde_json::Value::Array(parts))
}

fn text_of(blocks: &[starriver_kernel::llm::ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Zhipu/bigmodel coding relay: a request that carries image content parts to
/// a model the endpoint serves TEXT-ONLY is answered
/// `400 {"error":{"code":"1210","message":"messages.content.type 参数非法，取值
/// 范围 ['text']"}}` (observed live 2026-11-01: `glm-5.3` on
/// `/api/coding/paas/v4` rejects `image_url` parts; `glm-5.3-flash` accepts
/// the same request). Undeclared (`supports_vision: null`) models let the
/// provider arbitrate images — this recognizer is what its verdict looks
/// like, so the adapter can surface the capability problem and remedy
/// instead of a bare 400. Scoped tight to the 1210 shape to avoid
/// reclassifying unrelated 400s.
pub fn unsupported_image_rejection(status: u16, body: &str) -> bool {
    status == 400
        && body.contains("\"code\":\"1210\"")
        && body.contains("messages.content.type")
        && body.contains("['text']")
}

pub fn normalize_status(status: u16) -> &'static str {
    normalize_status_body(status, "")
}

/// BUG-167: parse a `Retry-After` response header (seconds → milliseconds);
/// parity with the OpenAI adapter so a provider-advised backoff window is
/// not discarded.
pub fn parse_retry_after(header: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    let value = header?.to_str().ok()?;
    let seconds: u64 = value.trim().parse().ok()?;
    Some(seconds * 1_000)
}

/// BUG-167: pull the provider request id from the conventional response
/// headers (Anthropic uses `request-id`; the AWS-style variants ride some
/// relays) so error reports reconcile against the provider log.
pub fn parse_request_id(headers: &reqwest::header::HeaderMap) -> Option<String> {
    const NAMES: [&str; 4] = [
        "request-id",
        "x-request-id",
        "x-amzn-requestid",
        "x-amz-request-id",
    ];
    // NB: `?` inside the loop would return on the FIRST absent header and
    // make the fallback names dead code — check each candidate explicitly.
    for name in NAMES {
        if let Some(value) = headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            return Some(value.to_string());
        }
    }
    None
}

/// WI-9: body-aware classification so a confirmed context-window overflow
/// carries `CONTEXT_WINDOW_EXCEEDED` (openai adapter parity) and can drive
/// the loop's condense-and-retry recovery.
pub fn normalize_status_body(status: u16, body: &str) -> &'static str {
    let lowered = body.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| lowered.contains(n));
    match status {
        401 | 403 => "AUTH",
            // BUG-102: a 429 whose body announces billing/quota exhaustion
            // ("insufficient balance"/gateway code 1113…) is NOT a
            // rate limit — retrying is pointless. Classify it as
            // QUOTA_EXHAUSTED (non-retryable) so the user sees the truth.
            429 if has(&["余额不足", "欠费", "无可用资源包", "insufficient balance",
                         "insufficient quota", "resource package", "\"code\":\"1113\""]) =>
            {
                "QUOTA_EXHAUSTED"
            }
        429 => "RATE_LIMIT",
        400 => {
            if has(&["context length", "context window", "maximum context"]) {
                "CONTEXT_WINDOW_EXCEEDED"
            } else {
                "INVALID_REQUEST"
            }
        }
        s if (500..600).contains(&s) => "SERVER",
        _ => "HTTP_ERROR",
    }
}

/// Map a provider stream-error type to the kernel error-code vocabulary
/// (BUG-162): these join the same retry classification the HTTP-status path
/// produces, so `overloaded_error` remains retryable SERVER and quota/auth
/// failures stay non-retryable.
fn stream_error_code(kind: &str) -> &'static str {
    match kind {
        "overloaded_error" | "api_error" => "SERVER",
        "timeout_error" => "TIMEOUT",
        "rate_limit_error" => "RATE_LIMIT",
        "authentication_error" | "permission_error" => "AUTH",
        "invalid_request_error" => "INVALID_REQUEST",
        _ => "SERVER",
    }
}

// ── SSE translator ──

struct OpenBlock {
    index: u64,
    kind: Kind,
    text: String,
    /// Thinking blocks only: the cryptographic signature Anthropic requires
    /// verbatim on replay.
    signature: String,
    call_id: String,
    name: String,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    ToolCall,
    Thinking,
}

#[allow(dead_code)]
pub struct SseTranslator {
    blocks: Vec<OpenBlock>,
    next_index: u64,
    pending_stop_reason: Option<String>,
    pending_usage: Option<TokenUsage>,
    /// `message_start` carries the real input count and cache split;
    /// `message_delta` usage only accumulates output_tokens.
    pending_input: Option<u64>,
    pending_cache_read: Option<u64>,
    pending_cache_write: Option<u64>,
    /// Server-reported model id from `message_start` (identity backfill):
    /// rides the finish chunk's replay envelope so callers can compare the
    /// configured model against the model that actually served the call.
    pending_model: Option<String>,
    done: bool,
}

impl SseTranslator {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            next_index: 0,
            pending_stop_reason: None,
            pending_usage: None,
            pending_input: None,
            pending_cache_read: None,
            pending_cache_write: None,
            pending_model: None,
            done: false,
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Feed one SSE frame's `data:` JSON payload.
    pub fn feed_frame(&mut self, json: &str) -> Vec<Result<StreamChunk, LlmError>> {
        if self.done {
            return Vec::new();
        }
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
            return out;
        };
        let event_type = v
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string();

        match event_type.as_str() {
            "content_block_start" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let block = v
                    .get("content_block")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match block_type {
                    "text" => {
                        self.blocks.push(OpenBlock {
                            index,
                            kind: Kind::Text,
                            text: String::new(),
                            signature: String::new(),
                            call_id: String::new(),
                            name: String::new(),
                        });
                        out.push(Ok(StreamChunk::BlockStart {
                            index,
                            block_type: ContentBlockType::Text,
                        }));
                    }
                    "thinking" => {
                        self.blocks.push(OpenBlock {
                            index,
                            kind: Kind::Thinking,
                            text: String::new(),
                            signature: String::new(),
                            call_id: String::new(),
                            name: String::new(),
                        });
                        out.push(Ok(StreamChunk::BlockStart {
                            index,
                            block_type: ContentBlockType::Custom(THINKING_BLOCK_KIND.into()),
                        }));
                    }
                    "tool_use" => {
                        let id = block
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or_default()
                            .to_string();
                        self.blocks.push(OpenBlock {
                            index,
                            kind: Kind::ToolCall,
                            text: String::new(),
                            signature: String::new(),
                            call_id: id.clone(),
                            name: name.clone(),
                        });
                        out.push(Ok(StreamChunk::BlockStart {
                            index,
                            block_type: ContentBlockType::ToolCall,
                        }));
                        out.push(Ok(StreamChunk::ToolCallDelta {
                            index,
                            id: CallId(id),
                            name: Some(name),
                            arguments_delta: String::new(),
                        }));
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let delta = v.get("delta").cloned().unwrap_or(serde_json::Value::Null);
                let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match delta_type {
                    "text_delta" => {
                        let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        if let Some(b) = self.blocks.iter_mut().find(|b| b.index == index) {
                            b.text.push_str(text);
                        }
                        out.push(Ok(StreamChunk::TextDelta {
                            index,
                            text: text.to_string(),
                        }));
                    }
                    "thinking_delta" => {
                        let text = delta.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                        if let Some(b) = self.blocks.iter_mut().find(|b| b.index == index) {
                            b.text.push_str(text);
                        }
                        // The display channel mirrors the thinking text as
                        // reasoning deltas; the verbatim capture rides the
                        // block end.
                        out.push(Ok(StreamChunk::ReasoningDelta {
                            index,
                            text: text.to_string(),
                        }));
                    }
                    "signature_delta" => {
                        let signature = delta
                            .get("signature")
                            .and_then(|s| s.as_str())
                            .unwrap_or("");
                        if let Some(b) = self.blocks.iter_mut().find(|b| b.index == index) {
                            b.signature.push_str(signature);
                        }
                    }
                    "input_json_delta" => {
                        let json = delta
                            .get("partial_json")
                            .and_then(|j| j.as_str())
                            .unwrap_or("");
                        if let Some(b) = self.blocks.iter_mut().find(|b| b.index == index) {
                            b.text.push_str(json);
                        }
                        let (call_id, _) = self
                            .blocks
                            .iter()
                            .find(|b| b.index == index)
                            .map(|b| (b.call_id.clone(), b.name.clone()))
                            .unwrap_or_default();
                        out.push(Ok(StreamChunk::ToolCallDelta {
                            index,
                            id: CallId(call_id),
                            name: None,
                            arguments_delta: json.to_string(),
                        }));
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                if let Some(pos) = self.blocks.iter().position(|b| b.index == index) {
                    let block = self.blocks.remove(pos);
                    out.push(Ok(StreamChunk::BlockEnd {
                        index: block.index,
                        block: close(block),
                    }));
                }
            }
            "message_start" => {
                // Input count and the cache split only appear here; the
                // later message_delta usage carries just output_tokens.
                if let Some(message) = v.get("message") {
                    if let Some(usage) = message.get("usage") {
                        self.pending_input = usage.get("input_tokens").and_then(|t| t.as_u64());
                        self.pending_cache_read = usage
                            .get("cache_read_input_tokens")
                            .and_then(|t| t.as_u64());
                        self.pending_cache_write = usage
                            .get("cache_creation_input_tokens")
                            .and_then(|t| t.as_u64());
                    }
                    self.pending_model = message
                        .get("model")
                        .and_then(|m| m.as_str())
                        .map(String::from);
                }
            }
            "message_delta" => {
                if let Some(delta) = v.get("delta") {
                    if let Some(reason) = delta.get("stop_reason").and_then(|s| s.as_str()) {
                        self.pending_stop_reason = Some(reason.to_string());
                    }
                }
                if let Some(usage) = v.get("usage") {
                    // Merge with message_start: delta's own values win when a
                    // provider repeats them, pending input fills the rest.
                    let input = usage
                        .get("input_tokens")
                        .and_then(|t| t.as_u64())
                        .or(self.pending_input)
                        .unwrap_or(0);
                    let cache_read = usage
                        .get("cache_read_input_tokens")
                        .and_then(|t| t.as_u64())
                        .or(self.pending_cache_read);
                    let cache_write = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|t| t.as_u64())
                        .or(self.pending_cache_write);
                    self.pending_usage = Some(TokenUsage {
                        input_tokens: input,
                        output_tokens: usage
                            .get("output_tokens")
                            .and_then(|t| t.as_u64())
                            .unwrap_or(0),
                        cache_read_tokens: cache_read,
                        cache_write_tokens: cache_write,
                        reasoning_tokens: None,
                    });
                }
            }
            "error" => {
                // BUG-162: a documented mid-stream error event
                // (overloaded_error, api_error, …) must surface as an error
                // carrying its provider type — the old `_ => {}` arm
                // swallowed it and the turn ended as a bare
                // PROVIDER_STREAM_TRUNCATED with the real cause lost.
                let error = v.get("error").cloned().unwrap_or(serde_json::Value::Null);
                let kind = error
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("api_error");
                let message = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("stream error");
                self.done = true;
                out.push(Err(LlmError::Call {
                    code: stream_error_code(kind).into(),
                    message: format!("stream error event {kind}: {message}"),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                }));
            }
            "message_stop" => {
                self.done = true;
                let reason = self
                    .pending_stop_reason
                    .as_deref()
                    .map(|s| match s {
                        "end_turn" => FinishReason::Stop,
                        "max_tokens" => FinishReason::MaxTokens,
                        "tool_use" => FinishReason::ToolCalls,
                        other => FinishReason::Error {
                            failure: starriver_kernel::llm::LlmFailure {
                                message: format!("model stopped: {other}"),
                                code: other.to_uppercase(),
                                status: None,
                                provider_retry_after_ms: None,
                                request_id: None,
                            },
                        },
                    })
                    .unwrap_or(FinishReason::Stop);
                let usage = self.pending_usage.take();
                if let Some(u) = usage {
                    out.push(Ok(StreamChunk::Usage { usage: u }));
                }
                out.push(Ok(StreamChunk::Finish {
                    reason,
                    replay_state: self.take_replay_state(),
                }));
            }
            _ => {}
        }
        out
    }

    /// The server-reported identity envelope riding the finish chunk, or
    /// `None` when the stream never announced a model.
    fn take_replay_state(&mut self) -> Option<ReplayEnvelope> {
        let model = self.pending_model.take()?;
        Some(ReplayEnvelope {
            response: JsonValue::Object(vec![("model".into(), JsonValue::String(model))]),
            blocks: None,
        })
    }

    /// Call on byte-stream end (without `message_stop`) to close open blocks.
    pub fn finish(&mut self) -> Vec<Result<StreamChunk, LlmError>> {
        self.done = true;
        let mut out = Vec::new();
        for block in self.blocks.drain(..) {
            out.push(Ok(StreamChunk::BlockEnd {
                index: block.index,
                block: close(block),
            }));
        }
        let reason = self
            .pending_stop_reason
            .as_deref()
            .map(|s| match s {
                "end_turn" => FinishReason::Stop,
                "max_tokens" => FinishReason::MaxTokens,
                "tool_use" => FinishReason::ToolCalls,
                other => FinishReason::Error {
                    failure: starriver_kernel::llm::LlmFailure {
                        message: format!("model stopped: {other}"),
                        code: other.to_uppercase(),
                        status: None,
                        provider_retry_after_ms: None,
                        request_id: None,
                    },
                },
            })
            // BUG-99: byte-stream end without message_stop = truncated
            // response — an error, never a clean stop.
            .unwrap_or_else(FinishReason::truncated_stream);
        if let Some(u) = self.pending_usage.take() {
            out.push(Ok(StreamChunk::Usage { usage: u }));
        }
        out.push(Ok(StreamChunk::Finish {
            reason,
            replay_state: self.take_replay_state(),
        }));
        out
    }
}

fn close(block: OpenBlock) -> ContentBlock {
    match block.kind {
        Kind::Text => ContentBlock::Text(block.text),
        Kind::ToolCall => ContentBlock::ToolCall {
            id: CallId(block.call_id),
            name: Some(block.name),
            // The one text→JsonValue parse at block close.
            arguments: starriver_kernel::llm::tool_call_arguments_from_text(&block.text),
        },
        Kind::Thinking => ContentBlock::Custom {
            kind: THINKING_BLOCK_KIND.into(),
            payload: JsonValue::Object(vec![
                ("type".into(), JsonValue::String("thinking".into())),
                ("thinking".into(), JsonValue::String(block.text)),
                ("signature".into(), JsonValue::String(block.signature)),
            ]),
        },
    }
}

fn json_to_serde(value: &starriver_kernel::json::JsonValue) -> serde_json::Value {
    use starriver_kernel::json::JsonValue as J;
    match value {
        J::Null => serde_json::Value::Null,
        J::Bool(b) => serde_json::Value::Bool(*b),
        J::Int(i) => serde_json::Value::Number((*i).into()),
        J::UInt(u) => serde_json::Value::Number((*u).into()),
        J::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        J::String(t) => serde_json::Value::String(t.clone()),
        J::Array(items) => serde_json::Value::Array(items.iter().map(json_to_serde).collect()),
        J::Object(entries) => serde_json::Value::Object(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), json_to_serde(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::llm::{GenerateOptions, Message, MessageId, MessageRole, MessageSource};

    #[test]
    fn glm_1210_image_rejection_is_recognized() {
        // Exact body observed live (2026-11-01, glm-5.3 @ coding relay).
        let body = r#"{"error":{"code":"1210","message":"messages.content.type 参数非法，取值范围 ['text']"}}"#;
        assert!(unsupported_image_rejection(400, body));
        // Scoped: other 400 bodies, other codes, missing markers → no match.
        assert!(!unsupported_image_rejection(
            400,
            r#"{"error":{"code":"1210","message":"参数非法"}}"#
        ));
        assert!(!unsupported_image_rejection(429, body));
        assert!(!unsupported_image_rejection(
            400,
            r#"{"error":{"code":"1301","message":"内容敏感"}}"#
        ));
    }

    fn options() -> GenerateOptions {
        GenerateOptions {
            provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            reasoning_effort: None,
            messages: vec![Message {
                id: MessageId("m1".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::Text("hello".into())],
                source: MessageSource::user(),
            }],
            system: Some("You are helpful.".into()),
            tools: None,
            temperature: None,
            max_tokens: Some(4096),
            stop: None,
            session_id: Some("s1".into()),
            agent_id: None,
            tool_choice: None,
            cancellation: None,
            purpose: None,
            loop_assembled: true,
        }
    }

    #[test]
    fn serialize_request_has_system_instructions_and_max_tokens() {
        let body = serialize_request(&options()).expect("serialize");
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["system"], "You are helpful.");

        assert_eq!(body["stream"], true);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        // System must NOT appear in messages (it goes in top-level `system`).
        assert!(msgs.iter().all(|m| m["role"] != "system"));
    }

    /// BUG-41 regression: `GenerateOptions.temperature`/`stop` are caller
    /// parameters; the Messages API natively supports them (`temperature`,
    /// `stop_sequences`). Silently dropping them froze every anthropic route
    /// at the provider default.
    #[test]
    fn serialize_request_carries_temperature_and_stop_sequences() {
        let mut opts = options();
        opts.temperature = Some(0.2);
        opts.stop = Some(vec!["END".into(), "STOP".into()]);
        let body = serialize_request(&opts).expect("serialize");
        assert_eq!(body["temperature"], 0.2, "temperature must ride the request");
        assert_eq!(
            body["stop_sequences"],
            serde_json::json!(["END", "STOP"]),
            "stop sequences must ride the request"
        );
    }

    #[test]
    fn serialize_request_with_tools() {
        let mut opts = options();
        opts.tools = Some(vec![starriver_kernel::llm::ToolSchema {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: starriver_kernel::json::JsonValue::Object(vec![]),
        }]);
        let body = serialize_request(&opts).expect("serialize");
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "read");
    }

    #[test]
    /// BUG-102: billing/quota exhaustion 429 is QUOTA_EXHAUSTED, not RATE_LIMIT.
    #[test]
    fn quota_exhaustion_429_is_not_rate_limit() {
        assert_eq!(
            normalize_status_body(429, r#"{"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}"#),
            "QUOTA_EXHAUSTED"
        );
        assert_eq!(normalize_status_body(429, "insufficient balance"), "QUOTA_EXHAUSTED");
        assert_eq!(normalize_status_body(429, "too many requests"), "RATE_LIMIT");
    }

    fn normalize_status_covers_all_classes() {
        assert_eq!(normalize_status(401), "AUTH");
        assert_eq!(normalize_status(403), "AUTH");
        assert_eq!(normalize_status(429), "RATE_LIMIT");
        assert_eq!(normalize_status(400), "INVALID_REQUEST");
        assert_eq!(normalize_status(500), "SERVER");
        assert_eq!(normalize_status(503), "SERVER");
        assert_eq!(normalize_status(418), "HTTP_ERROR");
    }

    #[test]
    fn sse_translator_text_lifecycle() {
        let mut t = SseTranslator::new();
        let out = t.feed_frame(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
        );
        assert!(out.iter().any(|c| matches!(
            c,
            Ok(StreamChunk::BlockStart {
                block_type: ContentBlockType::Text,
                ..
            })
        )));
        let out = t.feed_frame(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#);
        assert!(out
            .iter()
            .any(|c| matches!(c, Ok(StreamChunk::TextDelta { text, .. }) if text == "hello")));
        let out = t.feed_frame(r#"{"type":"content_block_stop","index":0}"#);
        assert!(out.iter().any(|c| matches!(
            c,
            Ok(StreamChunk::BlockEnd {
                block: ContentBlock::Text(_),
                ..
            })
        )));
        let out = t.feed_frame(r#"{"type":"message_stop"}"#);
        assert!(out.iter().any(|c| matches!(
            c,
            Ok(StreamChunk::Finish {
                reason: FinishReason::Stop,
                ..
            })
        )));
        assert!(t.is_done());
    }

    /// BUG-99: byte-stream end WITHOUT `message_stop` = truncated response —
    /// the synthesized finish must be an error, never a clean Stop.
    #[test]
    fn stream_end_without_message_stop_is_truncation_not_stop() {
        let mut t = SseTranslator::new();
        t.feed_frame(r#"{"type":"message_start","message":{"model":"claude-x"}}"#);
        t.feed_frame(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#);
        t.feed_frame(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#);
        t.feed_frame(r#"{"type":"content_block_stop","index":0}"#);
        let out = t.finish();
        let reason = out.iter().find_map(|c| match c {
            Ok(StreamChunk::Finish { reason, .. }) => Some(reason),
            _ => None,
        });
        match reason {
            Some(FinishReason::Error { failure }) => {
                assert_eq!(failure.code, "PROVIDER_STREAM_TRUNCATED");
            }
            other => panic!("truncated stream must surface an error, got {other:?}"),
        }
    }

    #[test]
    fn sse_translator_tool_use_lifecycle() {
        let mut t = SseTranslator::new();
        let _ = t.feed_frame(r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_1","name":"read"}}"#);
        let out = t.feed_frame(r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"x\"}"}}"#);
        assert!(out
            .iter()
            .any(|c| matches!(c, Ok(StreamChunk::ToolCallDelta { .. }))));
        let out = t.feed_frame(r#"{"type":"content_block_stop","index":0}"#);
        assert!(out.iter().any(|c| matches!(c, Ok(StreamChunk::BlockEnd { block: ContentBlock::ToolCall { id, name, .. }, .. }) if id.0 == "tu_1" && name.as_deref() == Some("read"))));
    }

    #[test]
    fn sse_translator_merges_message_start_usage_with_delta() {
        // Real API shape: input + cache split arrive on `message_start`;
        // `message_delta` usage carries only the accumulated output_tokens.
        let mut t = SseTranslator::new();
        let _ = t.feed_frame(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":25,"output_tokens":1,"cache_read_input_tokens":10,"cache_creation_input_tokens":3}}}"#,
        );
        let _ = t.feed_frame(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":50}}"#,
        );
        let out = t.finish();
        let usage = out
            .iter()
            .find_map(|c| match c {
                Ok(StreamChunk::Usage { usage }) => Some(usage),
                _ => None,
            })
            .expect("merged usage chunk");
        assert_eq!(usage.input_tokens, 25, "input must come from message_start");
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.cache_read_tokens, Some(10));
        assert_eq!(usage.cache_write_tokens, Some(3));
    }

    #[test]
    fn sse_translator_message_delta_sets_stop_reason_and_usage() {
        let mut t = SseTranslator::new();
        let _ = t.feed_frame(r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"input_tokens":100,"output_tokens":50}}"#);
        let out = t.finish();
        assert!(out.iter().any(|c| matches!(
            c,
            Ok(StreamChunk::Finish {
                reason: FinishReason::MaxTokens,
                ..
            })
        )));
        assert!(out.iter().any(|c| matches!(c, Ok(StreamChunk::Usage { usage, .. }) if usage.input_tokens == 100 && usage.output_tokens == 50)));
    }

    #[test]
    fn sse_translator_done_is_idempotent() {
        let mut t = SseTranslator::new();
        let _ = t.feed_frame(r#"{"type":"message_stop"}"#);
        assert!(t.is_done());
        let second = t.feed_frame(r#"{"type":"message_stop"}"#);
        assert!(second.is_empty(), "done translator must be silent");
    }

    // ── WI-30 ①: thinking blocks round-trip across tool-call turns ──

    #[test]
    fn sse_thinking_block_is_captured_with_signature() {
        let mut t = SseTranslator::new();
        let _ = t.feed_frame(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        );
        let out = t.feed_frame(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"let me think"}}"#,
        );
        assert!(
            out.iter()
                .any(|c| matches!(c, Ok(StreamChunk::ReasoningDelta { text, .. }) if text == "let me think")),
            "thinking deltas mirror onto the reasoning display channel"
        );
        let _ = t.feed_frame(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-abc"}}"#,
        );
        let out = t.feed_frame(r#"{"type":"content_block_stop","index":0}"#);
        let block = out
            .iter()
            .find_map(|c| match c {
                Ok(StreamChunk::BlockEnd { block, .. }) => Some(block.clone()),
                _ => None,
            })
            .expect("thinking block end");
        match block {
            ContentBlock::Custom { kind, payload } => {
                assert_eq!(kind, THINKING_BLOCK_KIND);
                assert_eq!(
                    payload.get("thinking").and_then(JsonValue::as_str),
                    Some("let me think")
                );
                assert_eq!(
                    payload.get("signature").and_then(JsonValue::as_str),
                    Some("sig-abc"),
                    "the signature must be preserved for replay"
                );
            }
            other => panic!("expected captured thinking block, got {other:?}"),
        }
    }

    #[test]
    fn thinking_block_replays_verbatim_with_tool_use_next_turn() {
        let thinking = ContentBlock::Custom {
            kind: THINKING_BLOCK_KIND.into(),
            payload: starriver_kernel::json::JsonValue::Object(vec![
                (
                    "type".into(),
                    starriver_kernel::json::JsonValue::String("thinking".into()),
                ),
                (
                    "thinking".into(),
                    starriver_kernel::json::JsonValue::String("ponder".into()),
                ),
                (
                    "signature".into(),
                    starriver_kernel::json::JsonValue::String("sig-1".into()),
                ),
            ]),
        };
        let mut opts = options();
        opts.messages = vec![Message {
            id: MessageId("m2".into()),
            role: MessageRole::Assistant,
            content: vec![
                thinking,
                ContentBlock::tool_call(
                    CallId("toolu_1".into()),
                    Some("bash".into()),
                    JsonValue::from_json_str(r#"{"command":"ls"}"#).unwrap(),
                ),
            ],
            source: MessageSource::Model {
                provider: "anthropic".into(),
                model: "claude-sonnet-4".into(),
                replay_state: None,
            },
        }];
        let body = serialize_request(&opts).expect("serialize");
        let messages = body["messages"].as_array().unwrap();
        let assistant = messages
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant turn must serialize");
        let content = assistant["content"].as_array().expect("block array");
        assert_eq!(content.len(), 2, "thinking + tool_use in stream order");
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "ponder");
        assert_eq!(content[0]["signature"], "sig-1");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["id"], "toolu_1");
        assert_eq!(content[1]["name"], "bash");
        assert_eq!(content[1]["input"]["command"], "ls");
    }

    #[test]
    fn plain_assistant_text_stays_a_string() {
        let mut opts = options();
        opts.messages = vec![
            Message {
                id: MessageId("m1".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::Text("go".into())],
                source: MessageSource::user(),
            },
            Message {
                id: MessageId("m2".into()),
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text("done".into())],
                source: MessageSource::Model {
                    provider: "anthropic".into(),
                    model: "claude-sonnet-4".into(),
                    replay_state: None,
                },
            },
        ];
        let body = serialize_request(&opts).expect("serialize");
        // BUG-170: an assistant-first history is seeded with a user turn, so
        // index by role rather than position.
        let assistant = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant turn present");
        assert_eq!(assistant["content"], "done");
    }

    #[test]
    fn tool_result_serializes_as_tool_result_block() {
        let mut opts = options();
        opts.messages = vec![Message {
            id: MessageId("m2".into()),
            role: MessageRole::User,
            content: vec![
                ContentBlock::ToolResult {
                    call_id: CallId("toolu_1".into()),
                    content: vec![ContentBlock::Text("file list".into())],
                    is_error: false,
                },
                ContentBlock::Text("and go on".into()),
            ],
            source: MessageSource::Tool {
                call_id: CallId("toolu_1".into()),
            },
        }];
        let body = serialize_request(&opts).expect("serialize");
        let content = body["messages"][0]["content"]
            .as_array()
            .expect("block array");
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "toolu_1");
        assert_eq!(content[0]["content"], "file list");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "and go on");
    }

    // ── WI-69: user-turn images ride the wire as base64 sources ──

    #[test]
    fn image_content_projects_to_base64_image_sources() {
        let mut opts = options();
        opts.messages = vec![Message {
            id: MessageId("m1".into()),
            role: MessageRole::User,
            content: vec![
                ContentBlock::Text("describe this".into()),
                ContentBlock::Image {
                    attachment: JsonValue::Object(vec![(
                        "url".into(),
                        JsonValue::String("data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==".into()),
                    )]),
                },
            ],
            source: MessageSource::user(),
        }];
        let body = serialize_request(&opts).expect("serialize");
        let user = &body["messages"][0];
        assert_eq!(user["role"], "user");
        let parts = user["content"].as_array().expect("block array");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "describe this");
        assert_eq!(parts[1]["type"], "image");
        assert_eq!(parts[1]["source"]["type"], "base64");
        assert_eq!(parts[1]["source"]["media_type"], "image/png");
        assert_eq!(parts[1]["source"]["data"], "iVBORw0KGgoAAAANSUhEUg==");
    }

    #[test]
    fn malformed_image_attachments_fail_loudly() {
        let message_with = |attachment| Message {
            id: MessageId("m1".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::Image { attachment }],
            source: MessageSource::user(),
        };
        for attachment in [
            // No url at all.
            JsonValue::Object(vec![(
                "ref".into(),
                JsonValue::String("sha256:abc".into()),
            )]),
            // An http ref cannot ride the base64 source.
            JsonValue::Object(vec![(
                "url".into(),
                JsonValue::String("https://cdn.example.com/x.png".into()),
            )]),
        ] {
            let mut opts = options();
            opts.messages = vec![message_with(attachment)];
            let err = serialize_request(&opts).expect_err("bad attachment must fail");
            assert_eq!(err.code(), "INVALID_ATTACHMENT_REF");
        }
    }

    // ── WI-30 ②: tool_choice passthrough ──

    fn tooled_options() -> GenerateOptions {
        let mut opts = options();
        opts.tools = Some(vec![starriver_kernel::llm::ToolSchema {
            name: "bash".into(),
            description: "Run a command".into(),
            parameters: starriver_kernel::json::JsonValue::Object(vec![]),
        }]);
        opts
    }

    #[test]
    fn tool_choice_defaults_to_auto_when_tools_present() {
        let body = serialize_request(&tooled_options()).expect("serialize");
        assert_eq!(body["tool_choice"], serde_json::json!({"type": "auto"}));
    }

    #[test]
    fn tool_choice_any_and_named_tool_pass_through() {
        let mut opts = tooled_options();
        opts.tool_choice = Some(ToolChoice::Any);
        let body = serialize_request(&opts).expect("serialize");
        assert_eq!(body["tool_choice"], serde_json::json!({"type": "any"}));

        opts.tool_choice = Some(ToolChoice::Tool {
            name: "bash".into(),
        });
        let body = serialize_request(&opts).expect("serialize");
        assert_eq!(
            body["tool_choice"],
            serde_json::json!({"type": "tool", "name": "bash"})
        );
    }

    #[test]
    fn tool_choice_is_omitted_without_tools() {
        let mut opts = options();
        opts.tool_choice = Some(ToolChoice::Any);
        let body = serialize_request(&opts).expect("serialize");
        assert!(body.get("tool_choice").is_none());
    }

    // ── WI-30 ③: multi-segment system ──

    #[test]
    fn single_system_segment_stays_a_string() {
        let body = serialize_request(&options()).expect("serialize");
        assert_eq!(body["system"], "You are helpful.");
    }

    #[test]
    fn system_role_messages_join_the_system_blocks() {
        let mut opts = options();
        opts.messages.insert(
            0,
            Message {
                id: MessageId("m0".into()),
                role: MessageRole::System,
                content: vec![ContentBlock::Text("Additional policy.".into())],
                source: MessageSource::user(),
            },
        );
        let body = serialize_request(&opts).expect("serialize");
        let system = body["system"]
            .as_array()
            .expect("multi-segment system array");
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "You are helpful.");
        assert_eq!(system[1]["type"], "text");
        assert_eq!(system[1]["text"], "Additional policy.");
        let messages = body["messages"].as_array().unwrap();
        assert!(
            messages.iter().all(|m| m["role"] != "system"),
            "System-role messages must not appear in messages"
        );
    }

    #[test]
    fn system_is_omitted_when_empty() {
        let mut opts = options();
        opts.system = None;
        let body = serialize_request(&opts).expect("serialize");
        assert!(body.get("system").is_none());
    }

    // ── WI-33: server-reported model identity backfill ──

    #[test]
    fn message_start_model_rides_the_finish_replay_state() {
        let mut t = SseTranslator::new();
        let _ = t.feed_frame(
            r#"{"type":"message_start","message":{"model":"claude-sonnet-4-20301010","usage":{"input_tokens":5,"output_tokens":1}}}"#,
        );
        let _ = t.feed_frame(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
        );
        let out = t.feed_frame(r#"{"type":"message_stop"}"#);
        let replay = out
            .iter()
            .find_map(|c| match c {
                Ok(StreamChunk::Finish { replay_state, .. }) => replay_state.clone(),
                _ => None,
            })
            .expect("finish chunk carries the replay envelope");
        assert_eq!(
            replay.response.get("model").and_then(JsonValue::as_str),
            Some("claude-sonnet-4-20301010"),
            "the server-reported model id must be exposed to callers"
        );
        assert!(replay.blocks.is_none());
    }

    #[test]
    fn finish_without_message_start_has_no_replay_state() {
        let mut t = SseTranslator::new();
        let out = t.finish();
        assert!(
            out.iter().all(|c| !matches!(
                c,
                Ok(StreamChunk::Finish {
                    replay_state: Some(_),
                    ..
                })
            )),
            "no model announcement means no identity envelope"
        );
    }
    // WI-47 Phase C lossless regression (§5-3): the rebuilt `tool_use.input`
    // carries exact values (u64 max) — one serialization, no re-parse loss.
    #[test]
    fn serialize_request_tool_use_input_is_lossless() {
        // Tool calls ride assistant messages (the realistic producer).
        let mut opts = options();
        let big = starriver_kernel::json::JsonValue::Object(vec![(
            "big".into(),
            starriver_kernel::json::JsonValue::UInt(18_446_744_073_709_551_615),
        )]);
        opts.messages = vec![Message {
            id: MessageId("m1".into()),
            role: MessageRole::Assistant,
            content: vec![ContentBlock::tool_call(
                CallId("toolu_1".into()),
                Some("bash".into()),
                big,
            )],
            source: MessageSource::Model {
                provider: "anthropic".into(),
                model: "claude-sonnet-4-20250514".into(),
                replay_state: None,
            },
        }];
        let body = serialize_request(&opts).expect("serialize");
        let msgs = body["messages"].as_array().unwrap();
        // BUG-170: an assistant-first history is seeded with a user turn, so
        // index by role rather than position.
        let assistant = msgs
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant turn present");
        let input = &assistant["content"][0]["input"];
        assert_eq!(input["big"], 18_446_744_073_709_551_615u64);
    }

    // ── BUG-163: effort → thinking.budget_tokens ──────────────────────

    #[test]
    fn effort_enables_thinking_with_budget_and_raises_max_tokens() {
        let mut opts = options();
        opts.reasoning_effort = Some("low".into());
        let body = serialize_request(&opts).expect("serialize");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 2_048);
        // 4096 default cap already exceeds the low budget: untouched.
        assert_eq!(body["max_tokens"], 4_096);

        opts.reasoning_effort = Some("high".into());
        let body = serialize_request(&opts).expect("serialize");
        assert_eq!(body["thinking"]["budget_tokens"], 8_192);
        // The Messages API requires max_tokens > budget_tokens — the default
        // cap is raised with headroom instead of failing the request.
        assert_eq!(body["max_tokens"], 8_192 + 2_048);
    }

    #[test]
    fn no_effort_or_off_leaves_thinking_to_the_provider() {
        let body = serialize_request(&options()).expect("serialize");
        assert!(body.get("thinking").is_none());

        let mut opts = options();
        opts.reasoning_effort = Some("off".into());
        let body = serialize_request(&opts).expect("serialize");
        assert!(
            body.get("thinking").is_none(),
            "\"off\" must not enable thinking"
        );
    }

    #[test]
    fn unknown_effort_fails_loudly() {
        let mut opts = options();
        opts.reasoning_effort = Some("medium".into());
        let err = serialize_request(&opts).expect_err("unknown effort must fail");
        assert_eq!(err.code(), "UNSUPPORTED_REASONING_EFFORT");
    }

    // ── BUG-170: role normalization ───────────────────────────────────

    #[test]
    fn consecutive_same_role_turns_are_merged() {
        let mut opts = options();
        opts.messages = vec![
            Message {
                id: MessageId("m1".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::Text("part one".into())],
                source: MessageSource::user(),
            },
            Message {
                id: MessageId("m2".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::Text("part two".into())],
                source: MessageSource::user(),
            },
        ];
        let body = serialize_request(&opts).expect("serialize");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "adjacent same-role turns fold into one");
        assert_eq!(msgs[0]["role"], "user");
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "part one");
        assert_eq!(content[1]["text"], "part two");
    }

    #[test]
    fn assistant_first_conversation_is_seeded_with_a_user_turn() {
        let mut opts = options();
        opts.messages = vec![Message {
            id: MessageId("m1".into()),
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text("replayed opening".into())],
            source: MessageSource::Model {
                provider: "anthropic".into(),
                model: "claude-sonnet-4-20250514".into(),
                replay_state: None,
            },
        }];
        let body = serialize_request(&opts).expect("serialize");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user", "first wire turn must be user");
        assert_eq!(msgs[1]["role"], "assistant");
    }

    // ── BUG-172: tool_result images ───────────────────────────────────

    #[test]
    fn tool_result_with_image_projects_content_array() {
        let mut opts = options();
        let attachment = JsonValue::Object(vec![(
            "url".into(),
            JsonValue::String("data:image/png;base64,aGk=".into()),
        )]);
        opts.messages = vec![Message {
            id: MessageId("m1".into()),
            role: MessageRole::User,
            content: vec![
                ContentBlock::ToolResult {
                    call_id: CallId("toolu_1".into()),
                    content: vec![
                        ContentBlock::Text("screenshot attached".into()),
                        ContentBlock::Image { attachment },
                    ],
                    is_error: false,
                },
            ],
            source: MessageSource::Tool {
                call_id: CallId("toolu_1".into()),
            },
        }];
        let body = serialize_request(&opts).expect("serialize");
        let part = &body["messages"][0]["content"][0];
        assert_eq!(part["type"], "tool_result");
        let content = part["content"].as_array().expect("image rides an array");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "aGk=");
    }

    // ── BUG-162: stream `error` event ─────────────────────────────────

    #[test]
    fn sse_error_event_surfaces_provider_error_and_stops() {
        let mut t = SseTranslator::new();
        let out = t.feed_frame(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        let error = out
            .iter()
            .find_map(|c| match c {
                Err(e) => Some(e.clone()),
                _ => None,
            })
            .expect("error event must surface as an error chunk");
        assert_eq!(error.code(), "SERVER");
        assert!(
            error.to_string().contains("overloaded_error"),
            "the provider error type must ride the message"
        );
        assert!(t.is_done(), "an error event terminates the stream");
    }

    #[test]
    fn sse_error_event_type_mapping_covers_retry_classes() {
        let mut t = SseTranslator::new();
        let out = t.feed_frame(r#"{"type":"error","error":{"type":"rate_limit_error"}}"#);
        assert!(out.iter().any(|c| matches!(c, Err(e) if e.code() == "RATE_LIMIT")));
        let mut t = SseTranslator::new();
        let out = t.feed_frame(r#"{"type":"error","error":{"type":"authentication_error"}}"#);
        assert!(out.iter().any(|c| matches!(c, Err(e) if e.code() == "AUTH")));
    }

    // ── BUG-167: Retry-After / request-id parsing ─────────────────────

    #[test]
    fn retry_after_parses_seconds_to_millis() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "7".parse().unwrap());
        assert_eq!(parse_retry_after(headers.get("retry-after")), Some(7_000));
        assert_eq!(parse_retry_after(None), None);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "soon".parse().unwrap());
        assert_eq!(
            parse_retry_after(headers.get("retry-after")),
            None,
            "non-numeric Retry-After is ignored, not fatal"
        );
    }

    #[test]
    fn request_id_falls_through_candidate_names() {
        // The first candidate is absent — the loop must keep looking
        // (the old `?`-in-loop returned None on the first miss).
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-request-id", "req_123".parse().unwrap());
        assert_eq!(parse_request_id(&headers).as_deref(), Some("req_123"));

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-amz-request-id", "amz_9".parse().unwrap());
        assert_eq!(
            parse_request_id(&headers).as_deref(),
            Some("amz_9"),
            "late candidates must still be found"
        );

        assert_eq!(parse_request_id(&reqwest::header::HeaderMap::new()), None);
    }
}
