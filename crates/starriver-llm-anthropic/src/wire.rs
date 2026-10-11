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
    let mut tools = Vec::new();
    for schema in options.tools.iter().flatten() {
        tools.push(serde_json::json!({
            "name": schema.name,
            "description": schema.description,
            "input_schema": json_to_serde(&schema.parameters),
        }));
    }
    let mut body = serde_json::json!({
        "model": options.model,
        "messages": messages,
        "max_tokens": options.max_tokens.unwrap_or(4096),
        "stream": true,
    });
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
                let mut part = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": call_id.0.clone(),
                });
                if !output.is_empty() {
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

pub fn normalize_status(status: u16) -> &'static str {
    normalize_status_body(status, "")
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

    fn options() -> GenerateOptions {
        GenerateOptions {
            provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            reasoning_effort: None,
            messages: vec![Message {
                id: MessageId("m1".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::Text("hello".into())],
                source: MessageSource::User,
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
        opts.messages = vec![Message {
            id: MessageId("m2".into()),
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text("done".into())],
            source: MessageSource::Model {
                provider: "anthropic".into(),
                model: "claude-sonnet-4".into(),
                replay_state: None,
            },
        }];
        let body = serialize_request(&opts).expect("serialize");
        assert_eq!(body["messages"][0]["content"], "done");
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
            source: MessageSource::User,
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
            source: MessageSource::User,
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
                source: MessageSource::User,
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
        assert_eq!(msgs[0]["role"], "assistant");
        let input = &msgs[0]["content"][0]["input"];
        assert_eq!(input["big"], 18_446_744_073_709_551_615u64);
    }
}
