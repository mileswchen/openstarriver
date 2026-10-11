//! Ollama wire: request serialization + NDJSON translation. User-turn image
//! blocks ride the native `images` message field as bare-base64 payloads (
//! attachment admission → inline data-URL projection).

use starriver_kernel::llm::{
    CallId, ContentBlock, ContentBlockType, FinishReason, GenerateOptions, LlmError, MessageRole,
    StreamChunk, TokenUsage,
};

use std::collections::HashMap;

pub fn client() -> reqwest::Client {
    reqwest::Client::new()
}

pub fn serialize_request(options: &GenerateOptions) -> Result<serde_json::Value, LlmError> {
    let mut messages = Vec::new();
    if let Some(system) = &options.system {
        messages.push(serde_json::json!({"role": "system", "content": system}));
    }
    for message in &options.messages {
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        };
        let content: String = message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut entry = serde_json::json!({"role": role, "content": content});
        // Ollama's native chat format carries images on the message as a
        // bare-base64 array (no data-URI prefix); only user turns produce
        // them.
        if matches!(message.role, MessageRole::User) {
            let images = collect_image_data(&message.content)?;
            if !images.is_empty() {
                entry["images"] = serde_json::json!(images);
            }
        }
        messages.push(entry);
    }
    let mut body =
        serde_json::json!({"model": options.model, "messages": messages, "stream": true});
    // Tool schemas MUST ride the request — without a `tools` key the model
    // never sees the definitions and tool calling is silently dead (openai/
    // anthropic adapters both serialize theirs).
    if let Some(tools) = &options.tools {
        if !tools.is_empty() {
            body["tools"] = serde_json::json!(tools
                .iter()
                .map(|schema| serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": schema.name,
                        "description": schema.description,
                        "parameters": schema.parameters.to_serde_json().unwrap_or_default(),
                    },
                }))
                .collect::<Vec<_>>());
        }
    }
    if let Some(max_tokens) = options.max_tokens {
        body["options"] = serde_json::json!({"num_predict": max_tokens});
    }
    if let Some(temp) = options.temperature {
        body["options"]["temperature"] = serde_json::json!(temp);
    }
    Ok(body)
}

/// Collect the bare-base64 payload of every `Image` block in a user turn, in
/// wire order. The attachment seam projects hot images to inline data URLs,
/// so anything else (missing url, http ref) cannot be projected and fails
/// loudly rather than silently dropping the image (
/// `INVALID_ATTACHMENT_REF`, openai-adapter parity).
fn collect_image_data(blocks: &[ContentBlock]) -> Result<Vec<String>, LlmError> {
    let invalid = |message: String| LlmError::Call {
        code: "INVALID_ATTACHMENT_REF".into(),
        message,
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    };
    let mut images = Vec::new();
    for block in blocks {
        let ContentBlock::Image { attachment } = block else {
            continue;
        };
        let url = match attachment {
            starriver_kernel::json::JsonValue::Object(entries) => entries
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
        let Some(rest) = url.strip_prefix("data:") else {
            return Err(invalid(
                "image attachment url must be an inline `data:` URL; an http ref cannot ride the `images` field"
                    .into(),
            ));
        };
        let Some((_, data)) = rest.split_once(";base64,") else {
            return Err(invalid(
                "image data URL must be `data:<media_type>;base64,<payload>`".into(),
            ));
        };
        if data.is_empty() {
            return Err(invalid("image data URL must carry a payload".into()));
        }
        images.push(data.to_string());
    }
    Ok(images)
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
        404 => "MODEL_NOT_FOUND",
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

struct OpenBlock {
    index: u64,
    kind: Kind,
    text: String,
    call_id: String,
    name: String,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    ToolCall,
}

pub struct NdjsonTranslator {
    blocks: Vec<OpenBlock>,
    tool_wire_to_block: HashMap<u32, u64>,
    next_index: u64,
    done: bool,
    pending_finish: Option<FinishReason>,
    pending_usage: Option<TokenUsage>,
}

impl Default for NdjsonTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl NdjsonTranslator {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            tool_wire_to_block: HashMap::new(),
            next_index: 0,
            done: false,
            pending_finish: None,
            pending_usage: None,
        }
    }
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Open a block if none of `kind` is active, emitting the protocol's
    /// required `BlockStart` for a fresh block (kernel chunks require a start
    /// before any delta; previously text blocks silently skipped it).
    fn ensure_open(&mut self, kind: Kind) -> (u64, Vec<Result<StreamChunk, LlmError>>) {
        if let Some(existing) = self.blocks.iter().find(|b| b.kind == kind) {
            return (existing.index, Vec::new());
        }
        let index = self.next_index;
        self.next_index += 1;
        self.blocks.push(OpenBlock {
            index,
            kind,
            text: String::new(),
            call_id: String::new(),
            name: String::new(),
        });
        let block_type = match kind {
            Kind::Text => ContentBlockType::Text,
            Kind::ToolCall => ContentBlockType::ToolCall,
        };
        (
            index,
            vec![Ok(StreamChunk::BlockStart { index, block_type })],
        )
    }

    fn ensure_tool_block(&mut self, wire_index: u32, id: &str, name: &str) -> u64 {
        if let Some(existing) = self.tool_wire_to_block.get(&wire_index) {
            return *existing;
        }
        let assigned = self.next_index;
        self.next_index += 1;
        self.tool_wire_to_block.insert(wire_index, assigned);
        self.blocks.push(OpenBlock {
            index: assigned,
            kind: Kind::ToolCall,
            text: String::new(),
            call_id: id.to_string(),
            name: name.to_string(),
        });
        assigned
    }

    /// Feed one NDJSON line; returns zero or more kernel chunks.
    pub fn feed_line(&mut self, line: &str) -> Vec<Result<StreamChunk, LlmError>> {
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return out;
        };

        // BUG-50: an error line is a TERMINAL failure — nothing (especially
        // not a fabricated Ok(Finish)) may follow it in the stream.
        if v.get("error").is_some() {
            self.done = true;
            let msg = v["error"]
                .as_str()
                .unwrap_or("unknown ollama error")
                .to_string();
            out.push(Err(LlmError::Call {
                code: "ENGINE".into(),
                message: msg,
                status: None,
                provider_retry_after_ms: None,
                request_id: None,
            }));
            return out;
        }

        if let Some(message) = v.get("message") {
            if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
                if !content.is_empty() {
                    let (index, starts) = self.ensure_open(Kind::Text);
                    out.extend(starts);
                    if let Some(b) = self.blocks.iter_mut().find(|b| b.index == index) {
                        b.text.push_str(content);
                    }
                    out.push(Ok(StreamChunk::TextDelta {
                        index,
                        text: content.to_string(),
                    }));
                }
            }
            if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
                // BUG-38: each tool_call owns its wire index — a hardcoded 0
                // made every call after the first reuse the first call's
                // (already closed) block, losing its id/name/arguments.
                for (wire_index, tc) in tool_calls.iter().enumerate() {
                    let id = tc
                        .get("id")
                        .and_then(|i| i.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4()));
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let arguments = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    // Ollama streams tool-call arguments as partial string
                    // fragments across MULTIPLE NDJSON chunks — never close a
                    // tool block per line: the close popped the block (the
                    // next fragment then re-emitted a BlockStart the kernel
                    // ignores, and close_last popped the wrong — usually the
                    // Text — block). Tool blocks close at done:true below.
                    let already_open = self
                        .blocks
                        .iter()
                        .any(|b| self.tool_wire_to_block.get(&(wire_index as u32)) == Some(&b.index));
                    let idx = self.ensure_tool_block(wire_index as u32, &id, &name);
                    if !already_open {
                        out.push(Ok(StreamChunk::BlockStart {
                            index: idx,
                            block_type: ContentBlockType::ToolCall,
                        }));
                        out.push(Ok(StreamChunk::ToolCallDelta {
                            index: idx,
                            id: CallId(id.clone()),
                            name: Some(name.clone()),
                            arguments_delta: String::new(),
                        }));
                    }
                    if let Some(args_str) = arguments.as_str() {
                        if !args_str.is_empty() {
                            out.push(Ok(StreamChunk::ToolCallDelta {
                                index: idx,
                                id: CallId(String::new()),
                                name: None,
                                arguments_delta: args_str.to_string(),
                            }));
                            if let Some(b) = self.blocks.iter_mut().find(|b| b.index == idx) {
                                b.text.push_str(args_str);
                            }
                        }
                    }
                }
            }
        }

        if v.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
            let mut close_out = Vec::new();
            for block in self.blocks.drain(..) {
                close_out.push(Ok(StreamChunk::BlockEnd {
                    index: block.index,
                    block: close_block(block),
                }));
            }
            // BUG-39: the terminal frame's done_reason carries the real
            // finish cause — "length" means the output was cut by the token
            // limit and the kernel must drop incomplete tool calls.
            let reason = match v.get("done_reason").and_then(|r| r.as_str()) {
                Some("length") => FinishReason::MaxTokens,
                _ => self.pending_finish.take().unwrap_or(FinishReason::Stop),
            };
            // The terminal `done:true` object carries the token counts
            // (prompt_eval_count = input, eval_count = output).
            let usage = self.pending_usage.take().unwrap_or(TokenUsage {
                input_tokens: v
                    .get("prompt_eval_count")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0),
                output_tokens: v.get("eval_count").and_then(|t| t.as_u64()).unwrap_or(0),
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            });
            close_out.push(Ok(StreamChunk::Usage { usage }));
            close_out.push(Ok(StreamChunk::Finish {
                reason,
                replay_state: None,
            }));
            self.done = true;
            out.extend(close_out);
        }
        out
    }

    /// Call on stream end to close open blocks and emit finish.
    pub fn finish(&mut self) -> Vec<Result<StreamChunk, LlmError>> {
        // BUG-50: a stream that already terminated (terminal error) must not
        // fabricate a trailing finish — the error is the last word.
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let mut out = Vec::new();
        for block in self.blocks.drain(..) {
            out.push(Ok(StreamChunk::BlockEnd {
                index: block.index,
                block: close_block(block),
            }));
        }
        let reason = self.pending_finish.take().unwrap_or(FinishReason::Stop);
        if let Some(u) = self.pending_usage.take() {
            out.push(Ok(StreamChunk::Usage { usage: u }));
        }
        out.push(Ok(StreamChunk::Finish {
            reason,
            replay_state: None,
        }));
        out
    }
}

fn close_block(b: OpenBlock) -> ContentBlock {
    match b.kind {
        Kind::Text => ContentBlock::Text(b.text),
        Kind::ToolCall => ContentBlock::ToolCall {
            id: CallId(b.call_id),
            name: Some(b.name),
            // The one text→JsonValue parse at block close.
            arguments: starriver_kernel::llm::tool_call_arguments_from_text(&b.text),
        },
    }
}

fn close_last(blocks: &mut Vec<OpenBlock>) -> ContentBlock {
    blocks
        .pop()
        .map(close_block)
        .unwrap_or(ContentBlock::Text(String::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::json::JsonValue;
    use starriver_kernel::llm::{
        GenerateOptions, Message, MessageId, MessageRole, MessageSource,
    };

    fn options(messages: Vec<Message>) -> GenerateOptions {
        GenerateOptions {
            provider: "ollama".into(),
            model: "llava:13b".into(),
            reasoning_effort: None,
            messages,
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

    // ── WI-69: user-turn images ride the native `images` field ──

    #[test]
    fn user_images_ride_the_wire_as_bare_base64() {
        let messages = vec![Message {
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
                ContentBlock::Image {
                    attachment: JsonValue::Object(vec![(
                        "url".into(),
                        JsonValue::String("data:image/jpeg;base64,qqqq".into()),
                    )]),
                },
            ],
            source: MessageSource::User,
        }];

        let body = serialize_request(&options(messages)).expect("serialize");
        let user = &body["messages"][1];
        assert_eq!(user["role"], "user");
        assert_eq!(user["content"], "describe this");
        // Ollama wants bare base64 — the data-URI prefix must be stripped.
        let images = user["images"].as_array().expect("images array");
        assert_eq!(images.len(), 2);
        assert_eq!(images[0], "iVBORw0KGgoAAAANSUhEUg==");
        assert_eq!(images[1], "qqqq");
    }

    #[test]
    fn text_only_messages_carry_no_images_field() {
        let body = serialize_request(&options(Vec::new())).expect("serialize");
        assert!(body["messages"][0].get("images").is_none());
    }

    #[test]
    fn malformed_image_attachments_fail_loudly() {
        for attachment in [
            // No url at all.
            JsonValue::Object(vec![(
                "ref".into(),
                JsonValue::String("sha256:abc".into()),
            )]),
            // An http ref cannot be projected to bare base64.
            JsonValue::Object(vec![(
                "url".into(),
                JsonValue::String("https://cdn.example.com/x.png".into()),
            )]),
        ] {
            let messages = vec![Message {
                id: MessageId("m1".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::Image { attachment }],
                source: MessageSource::User,
            }];
            let err = serialize_request(&options(messages)).expect_err("bad attachment must fail");
            assert_eq!(err.code(), "INVALID_ATTACHMENT_REF");
        }
    }
}

// ── BUG-38 / BUG-39 regressions (round 4) ──
#[cfg(test)]
mod bug_round4_tests {
    use super::*;
    use starriver_kernel::llm::{ContentBlock, FinishReason, StreamChunk};

    fn drain(translator: &mut NdjsonTranslator, lines: &[&str]) -> Vec<Result<StreamChunk, LlmError>> {
        let mut out = Vec::new();
        for line in lines {
            out.extend(translator.feed_line(line));
        }
        out.extend(translator.finish());
        out
    }

    /// BUG-38: each tool_call in a message is its own wire item — the wire
    /// index must come from the iteration, not be hardcoded to 0 (the second
    /// call then reused the first call's closed block, lost its id/name/args,
    /// and emitted a mismatched BlockEnd).
    #[test]
    fn two_tool_calls_in_one_line_yield_two_tool_blocks() {
        let mut t = NdjsonTranslator::new();
        let chunks = drain(
            &mut t,
            &[r#"{"message":{"role":"assistant","content":"hmm","tool_calls":[
                {"function":{"name":"bash","arguments":"{\"a\":1}"}},
                {"function":{"name":"read","arguments":"{\"b\":2}"}}
            ]}}"#],
        );
        let mut tool_blocks = Vec::new();
        for chunk in &chunks {
            if let Ok(StreamChunk::BlockEnd { block, .. }) = chunk {
                tool_blocks.push(block.clone());
            }
        }
        let bash = tool_blocks.iter().any(|b| matches!(b,
            ContentBlock::ToolCall { name, arguments, .. }
                if name.as_deref() == Some("bash") && arguments.to_json_string().unwrap_or_default().contains("1")));
        let read = tool_blocks.iter().any(|b| matches!(b,
            ContentBlock::ToolCall { name, arguments, .. }
                if name.as_deref() == Some("read") && arguments.to_json_string().unwrap_or_default().contains("2")));
        assert!(bash, "first tool call (bash) must survive: {tool_blocks:?}");
        assert!(read, "second tool call (read) must survive: {tool_blocks:?}");
    }

    /// BUG-39: the terminal frame's done_reason must map — "length" means the
    /// output was cut by the token limit, and the kernel drops incomplete
    /// tool calls only under MaxTokens.
    #[test]
    fn done_reason_length_maps_to_max_tokens() {
        let mut t = NdjsonTranslator::new();
        let chunks = drain(
            &mut t,
            &[r#"{"done":true,"done_reason":"length","eval_count":3}"#],
        );
        let finish = chunks
            .iter()
            .filter_map(|c| match c {
                Ok(StreamChunk::Finish { reason, .. }) => Some(reason.clone()),
                _ => None,
            })
            .next()
            .expect("finish chunk present");
        assert_eq!(finish, FinishReason::MaxTokens);
        // stop (and absent) still map to Stop.
        let mut t2 = NdjsonTranslator::new();
        let chunks2 = drain(&mut t2, &[r#"{"done":true,"done_reason":"stop","eval_count":3}"#]);
        let f2 = chunks2
            .iter()
            .filter_map(|c| match c {
                Ok(StreamChunk::Finish { reason, .. }) => Some(reason.clone()),
                _ => None,
            })
            .next()
            .unwrap();
        assert_eq!(f2, FinishReason::Stop);
    }
}

// ── BUG-50 regression (round 5): error lines are terminal ──
#[cfg(test)]
mod bug50_tests {
    use super::*;
    use starriver_kernel::llm::{FinishReason, StreamChunk};

    /// The kernel contract: an Err item is a terminal failure — nothing
    /// (especially not a fabricated `Ok(Finish{Stop})`) may follow it, or
    /// consumers reading the LAST chunk mistake an engine crash for a normal
    /// completion.
    #[test]
    fn error_line_terminates_the_stream() {
        let mut t = NdjsonTranslator::new();
        let mut chunks = Vec::new();
        chunks.extend(t.feed_line(r#"{"error":"model crashed"}"#));
        chunks.extend(t.finish());
        let last = chunks.last().expect("non-empty");
        assert!(
            last.is_err(),
            "the stream must END on the error, not a fabricated finish: {chunks:?}"
        );
        assert!(
            !chunks.iter().any(|c| matches!(c,
                Ok(StreamChunk::Finish { reason: FinishReason::Stop, .. }))),
            "no Ok(Finish) may trail a terminal error: {chunks:?}"
        );
    }
}
