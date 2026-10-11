//! OpenAI Responses API: request serialization + typed SSE event translation
//! (LLP L2b), modeled on `llm-pi-ai`.
//!
//! Key differences from chat completions:
//! - system prompt → top-level `instructions`
//! - input is a list of typed items (message / function_call /
//!   function_call_output / reasoning)
//! - assistant `function_call` and `reasoning` items MUST be replayed verbatim
//!   on the next turn (reasoning loss rejects subsequent turns)
//! - output cap is `max_output_tokens`; `stop` is unsupported
//! - usage arrives on `response.completed`

use starriver_kernel::llm::{
    CallId, ContentBlock, ContentBlockType, FinishReason, GenerateOptions, LlmError, MessageRole,
    StreamChunk, TokenUsage,
};

use crate::{translate::StreamTranslator, ProviderProfile};

/// Kernel `Custom` block kind carrying the raw responses reasoning item, so
/// the item survives the session and can be replayed verbatim next turn.
pub const REASONING_ITEM_KIND: &str = "openai.reasoning_item";
/// Kernel `Custom` block kind carrying the raw responses item JSON.
const NO_OUTPUT_PLACEHOLDER: &str = "(no output)";

pub fn serialize_request(
    options: &GenerateOptions,
    profile: &ProviderProfile,
) -> Result<serde_json::Value, LlmError> {
    if options.stop.as_ref().is_some_and(|stop| !stop.is_empty()) {
        return Err(LlmError::Call {
            code: "UNSUPPORTED_OPTION".into(),
            message: "the OpenAI Responses API does not support `stop` sequences".into(),
            status: None,
            provider_retry_after_ms: None,
            request_id: None,
        });
    }
    let mut input = Vec::new();
    let mut instruction_parts: Vec<String> = Vec::new();
    if let Some(system) = &options.system {
        instruction_parts.push(system.clone());
    }

    for message in &options.messages {
        match message.role {
            MessageRole::System => {
                instruction_parts.push(flatten_text(&message.content));
            }
            MessageRole::User => {
                let text = flatten_text(&message.content);
                let images = crate::wire::collect_image_urls(&message.content)?;
                let tool_results: Vec<_> = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolResult {
                            call_id, content, ..
                        } => Some((call_id, content)),
                        _ => None,
                    })
                    .collect();
                // BUG-48: images alone still carry the turn.
                if !text.is_empty() || tool_results.is_empty() || !images.is_empty() {
                    let mut content_parts =
                        vec![serde_json::json!({"type": "input_text", "text": text})];
                    for url in images {
                        content_parts.push(serde_json::json!({
                            "type": "input_image",
                            "image_url": url,
                            "detail": "auto",
                        }));
                    }
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "user",
                        "content": content_parts,
                    }));
                }
                for (call_id, content) in tool_results {
                    let output = flatten_text(content);
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id.0.clone(),
                        "output": if output.is_empty() {
                            NO_OUTPUT_PLACEHOLDER.to_string()
                        } else {
                            output
                        },
                    }));
                }
            }
            MessageRole::Assistant => {
                let text = flatten_text(&message.content);
                if !text.is_empty() {
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
                for block in &message.content {
                    match block {
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => {
                            input.push(serde_json::json!({
                                "type": "function_call",
                                "call_id": id.0.clone(),
                                "name": name.clone().unwrap_or_default(),
                                // Responses wire format requires string
                                // arguments — one serialization here.
                                "arguments": arguments
                                    .to_json_string()
                                    .unwrap_or_default(),
                            }));
                        }
                        ContentBlock::Custom { kind, payload } if kind == REASONING_ITEM_KIND => {
                            // The reasoning item is replayed verbatim from the
                            // captured payload (OpenAI requires it).
                            input.push(json_to_serde(payload));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    let mut tools = Vec::new();
    for schema in options.tools.iter().flatten() {
        tools.push(serde_json::json!({
            "type": "function",
            "name": schema.name,
            "description": schema.description,
            "parameters": json_to_serde(&schema.parameters),
        }));
    }

    let mut body = serde_json::json!({
        "model": options.model,
        "input": input,
        "stream": true,
        "store": false,
    });
    let instructions = instruction_parts.join("\n");
    if !instructions.is_empty() {
        body["instructions"] = serde_json::json!(instructions);
    }
    if !tools.is_empty() {
        body["tools"] = serde_json::json!(tools);
    }
    if let Some(max) = profile.max_tokens.or(options.max_tokens) {
        body["max_output_tokens"] = serde_json::json!(max);
    }
    if let Some(temp) = options.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    // BUG-54: the profile's reasoning effort is advertised by resolve_model —
    // the responses route must carry it, not silently drop it.
    if let Some(effort) = options.reasoning_effort.as_deref().filter(|e| !e.is_empty()) {
        body["reasoning"] = serde_json::json!({ "effort": effort });
    }
    Ok(body)
}

fn flatten_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        // BUG-52: block boundaries are content boundaries — bare
        // concatenation fused adjacent segments ("note"+"go on" →
        // "notego on"); anthropic/ollama join with "\n".
        .collect::<Vec<_>>()
        .join("\n")
}

/// One streaming event from the responses API (subset we care about).
#[derive(Debug, Clone)]
enum ResponseEvent {
    /// `response.output_item.added` — `function_call` or `reasoning` item.
    ItemAdded {
        item_type: String,
        call_id: Option<String>,
        name: Option<String>,
    },
    /// `response.output_text.delta`.
    TextDelta(String),
    /// `response.output_text.done`.
    TextDone(String),
    /// `response.function_call_arguments.delta`.
    FunctionCallDelta(String),
    /// `response.output_item.done` — `function_call`.
    FunctionCallDone {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// `response.reasoning_summary_text.delta` (display channel).
    ReasoningDelta(String),
    /// `response.output_item.done` — `reasoning` (full item for passback).
    ReasoningItemDone(serde_json::Value),
    /// `response.completed` — carries usage and status.
    Completed {
        usage: Option<serde_json::Value>,
        status: String,
    },
}

/// Parse one SSE `data:` payload into a [`ResponseEvent`], if recognized.
fn parse_event(json: &serde_json::Value) -> Option<ResponseEvent> {
    let event_type = json.get("type")?.as_str()?;
    match event_type {
        "response.output_text.delta" => {
            let delta = json.get("delta")?.as_str()?.to_string();
            Some(ResponseEvent::TextDelta(delta))
        }
        "response.output_text.done" => {
            let text = json.get("text").and_then(|t| t.as_str()).unwrap_or("");
            Some(ResponseEvent::TextDone(text.to_string()))
        }
        "response.reasoning_summary_text.delta" => {
            let delta = json.get("delta")?.as_str()?.to_string();
            Some(ResponseEvent::ReasoningDelta(delta))
        }
        "response.output_item.added" => {
            let item = json.get("item")?;
            let item_type = item.get("type")?.as_str()?.to_string();
            match item_type.as_str() {
                "function_call" => Some(ResponseEvent::ItemAdded {
                    item_type,
                    call_id: item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    name: item.get("name").and_then(|v| v.as_str()).map(String::from),
                }),
                "reasoning" => Some(ResponseEvent::ItemAdded {
                    item_type,
                    call_id: None,
                    name: None,
                }),
                _ => None,
            }
        }
        "response.function_call_arguments.delta" => {
            let delta = json.get("delta")?.as_str()?.to_string();
            Some(ResponseEvent::FunctionCallDelta(delta))
        }
        "response.output_item.done" => {
            let item = json.get("item")?;
            match item.get("type")?.as_str()? {
                "function_call" => Some(ResponseEvent::FunctionCallDone {
                    call_id: item.get("call_id")?.as_str()?.to_string(),
                    name: item.get("name")?.as_str()?.to_string(),
                    arguments: item
                        .get("arguments")
                        .and_then(|a| a.as_str())
                        .unwrap_or_default()
                        .to_string(),
                }),
                "reasoning" => Some(ResponseEvent::ReasoningItemDone(item.clone())),
                _ => None,
            }
        }
        // BUG-47: the Responses API streams its truncation terminal as
        // `response.incomplete` and hard failures as `response.failed` (not
        // `completed` + status) — both carry the same response payload and
        // must reach the same status mapping below.
        "response.completed" | "response.incomplete" | "response.failed" => {
            let response = json.get("response")?;
            let usage = response.get("usage").cloned();
            let status = response
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("completed")
                .to_string();
            Some(ResponseEvent::Completed { usage, status })
        }
        _ => None,
    }
}

/// Stateful event → kernel chunk translator for the responses event stream.
pub struct ResponsesTranslator {
    next_index: u64,
    text_index: Option<u64>,
    text: String,
    tool_index: Option<u64>,
    tool_call_id: String,
    tool_name: String,
    tool_arguments: String,
    reasoning_index: Option<u64>,
    reasoning_text: String,
    reasoning_item_index: Option<u64>,
    reasoning_item: Option<serde_json::Value>,
    pending_usage: Option<TokenUsage>,
    finish: Option<FinishReason>,
    done: bool,
}

impl Default for ResponsesTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesTranslator {
    pub fn new() -> Self {
        Self {
            next_index: 0,
            text_index: None,
            text: String::new(),
            tool_index: None,
            tool_call_id: String::new(),
            tool_name: String::new(),
            tool_arguments: String::new(),
            reasoning_index: None,
            reasoning_text: String::new(),
            reasoning_item_index: None,
            reasoning_item: None,
            pending_usage: None,
            finish: None,
            done: false,
        }
    }

    fn allocate(&mut self) -> u64 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn apply(&mut self, event: ResponseEvent) -> Vec<Result<StreamChunk, LlmError>> {
        let mut out = Vec::new();
        match event {
            ResponseEvent::ItemAdded {
                item_type,
                call_id,
                name,
            } => {
                if item_type == "function_call" {
                    let index = self.allocate();
                    self.tool_index = Some(index);
                    self.tool_call_id = call_id.unwrap_or_default();
                    self.tool_name = name.unwrap_or_default();
                    self.tool_arguments.clear();
                    out.push(Ok(StreamChunk::BlockStart {
                        index,
                        block_type: ContentBlockType::ToolCall,
                    }));
                    out.push(Ok(StreamChunk::ToolCallDelta {
                        index,
                        id: CallId(self.tool_call_id.clone()),
                        name: Some(self.tool_name.clone()),
                        arguments_delta: String::new(),
                    }));
                } else if item_type == "reasoning" {
                    let index = self.allocate();
                    self.reasoning_item_index = Some(index);
                    out.push(Ok(StreamChunk::BlockStart {
                        index,
                        block_type: ContentBlockType::Custom(REASONING_ITEM_KIND.into()),
                    }));
                }
            }
            ResponseEvent::TextDelta(delta) => {
                let index = match self.text_index {
                    Some(index) => index,
                    None => {
                        let index = self.allocate();
                        self.text_index = Some(index);
                        out.push(Ok(StreamChunk::BlockStart {
                            index,
                            block_type: ContentBlockType::Text,
                        }));
                        index
                    }
                };
                self.text.push_str(&delta);
                out.push(Ok(StreamChunk::TextDelta { index, text: delta }));
            }
            ResponseEvent::TextDone(full) => {
                // BUG-53: proxies may deliver only the done event (no
                // deltas) — the done payload IS the final text and must not
                // be dropped when no delta ever opened the block.
                if let Some(index) = self.text_index.take() {
                    let text = std::mem::take(&mut self.text);
                    out.push(Ok(StreamChunk::BlockEnd {
                        index,
                        block: ContentBlock::Text(text),
                    }));
                } else if !full.is_empty() {
                    let index = self.allocate();
                    out.push(Ok(StreamChunk::BlockStart {
                        index,
                        block_type: ContentBlockType::Text,
                    }));
                    out.push(Ok(StreamChunk::BlockEnd {
                        index,
                        block: ContentBlock::Text(full.clone()),
                    }));
                }
            }
            ResponseEvent::FunctionCallDelta(delta) => {
                if let Some(index) = self.tool_index {
                    self.tool_arguments.push_str(&delta);
                    out.push(Ok(StreamChunk::ToolCallDelta {
                        index,
                        id: CallId(String::new()),
                        name: None,
                        arguments_delta: delta,
                    }));
                }
            }
            ResponseEvent::FunctionCallDone {
                call_id,
                name,
                arguments,
            } => {
                if let Some(index) = self.tool_index.take() {
                    out.push(Ok(StreamChunk::BlockEnd {
                        index,
                        block: ContentBlock::ToolCall {
                            id: CallId(call_id),
                            name: Some(name),
                            arguments: starriver_kernel::llm::tool_call_arguments_from_text(
                                &arguments,
                            ),
                        },
                    }));
                }
            }
            ResponseEvent::ReasoningDelta(delta) => {
                let index = match self.reasoning_index {
                    Some(index) => index,
                    None => {
                        let index = self.allocate();
                        self.reasoning_index = Some(index);
                        out.push(Ok(StreamChunk::BlockStart {
                            index,
                            block_type: ContentBlockType::Reasoning,
                        }));
                        index
                    }
                };
                self.reasoning_text.push_str(&delta);
                out.push(Ok(StreamChunk::ReasoningDelta { index, text: delta }));
            }
            ResponseEvent::ReasoningItemDone(item) => {
                // Close the display reasoning channel if it is still open.
                if let Some(index) = self.reasoning_index.take() {
                    let text = std::mem::take(&mut self.reasoning_text);
                    out.push(Ok(StreamChunk::BlockEnd {
                        index,
                        block: ContentBlock::Reasoning(text),
                    }));
                }
                if let Some(index) = self.reasoning_item_index.take() {
                    out.push(Ok(StreamChunk::BlockEnd {
                        index,
                        block: ContentBlock::Custom {
                            kind: REASONING_ITEM_KIND.into(),
                            payload: serde_json_to_kernel(item),
                        },
                    }));
                }
            }
            ResponseEvent::Completed { usage, status } => {
                if let Some(usage) = usage {
                    self.pending_usage = Some(map_usage(&usage));
                }
                self.finish = Some(match status.as_str() {
                    "completed" => FinishReason::Stop,
                    "incomplete" => FinishReason::MaxTokens,
                    other => FinishReason::Error {
                        failure: starriver_kernel::llm::LlmFailure {
                            message: format!("model response failed: {other}"),
                            code: "MODEL_ERROR".into(),
                            status: None,
                            provider_retry_after_ms: None,
                            request_id: None,
                        },
                    },
                });
                out.extend(self.emit_finish());
                self.done = true;
            }
        }
        out
    }

    fn emit_finish(&mut self) -> Vec<Result<StreamChunk, LlmError>> {
        let mut out = Vec::new();
        if let Some(index) = self.text_index.take() {
            let text = std::mem::take(&mut self.text);
            out.push(Ok(StreamChunk::BlockEnd {
                index,
                block: ContentBlock::Text(text),
            }));
        }
        if let Some(index) = self.reasoning_index.take() {
            let text = std::mem::take(&mut self.reasoning_text);
            out.push(Ok(StreamChunk::BlockEnd {
                index,
                block: ContentBlock::Reasoning(text),
            }));
        }
        if let Some(index) = self.tool_index.take() {
            out.push(Ok(StreamChunk::BlockEnd {
                index,
                block: ContentBlock::ToolCall {
                    id: CallId(std::mem::take(&mut self.tool_call_id)),
                    name: Some(std::mem::take(&mut self.tool_name)),
                    arguments: starriver_kernel::llm::tool_call_arguments_from_text(
                        &std::mem::take(&mut self.tool_arguments),
                    ),
                },
            }));
        }
        if let Some(index) = self.reasoning_item_index.take() {
            if let Some(item) = self.reasoning_item.take() {
                out.push(Ok(StreamChunk::BlockEnd {
                    index,
                    block: ContentBlock::Custom {
                        kind: REASONING_ITEM_KIND.into(),
                        payload: serde_json_to_kernel(item),
                    },
                }));
            }
        }
        if let Some(usage) = self.pending_usage.take() {
            out.push(Ok(StreamChunk::Usage { usage }));
        }
        // BUG-99: stream end without a finish reason = truncated response.
        if let Some(reason) = self.finish.take().or_else(|| Some(FinishReason::truncated_stream())) {
            out.push(Ok(StreamChunk::Finish {
                reason,
                replay_state: None,
            }));
        }
        out
    }
}

impl StreamTranslator for ResponsesTranslator {
    fn is_done(&self) -> bool {
        self.done
    }

    fn feed(&mut self, payload: &str) -> Vec<Result<StreamChunk, LlmError>> {
        if self.done {
            return Vec::new();
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(payload) else {
            return Vec::new();
        };
        match parse_event(&json) {
            Some(event) => self.apply(event),
            None => Vec::new(),
        }
    }

    fn finish(&mut self) -> Vec<Result<StreamChunk, LlmError>> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        self.emit_finish()
    }
}

/// Map usage from the responses API shape to kernel TokenUsage.
pub fn map_usage(usage: &serde_json::Value) -> TokenUsage {
    let num = |key: &str| usage.get(key).and_then(|v| v.as_u64());
    let detail = |container: &str, key: &str| {
        usage
            .get(container)
            .and_then(|d| d.get(key))
            .and_then(|v| v.as_u64())
    };
    // BUG-51: `input_tokens` includes cached tokens (same wire semantics as
    // completions `prompt_tokens`) — the net input must subtract the cache
    // read or the token meter double-counts every cached token.
    let cache_read = detail("input_tokens_details", "cached_tokens");
    TokenUsage {
        input_tokens: num("input_tokens")
            .unwrap_or(0)
            .saturating_sub(cache_read.unwrap_or(0)),
        output_tokens: num("output_tokens").unwrap_or(0),
        cache_read_tokens: cache_read,
        cache_write_tokens: None,
        reasoning_tokens: detail("output_tokens_details", "reasoning_tokens"),
    }
}

/// Recursive kernel JsonValue → serde_json::Value conversion.
pub(crate) fn json_to_serde(value: &starriver_kernel::json::JsonValue) -> serde_json::Value {
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

fn serde_json_to_kernel(value: serde_json::Value) -> starriver_kernel::json::JsonValue {
    use starriver_kernel::json::JsonValue as J;
    match value {
        serde_json::Value::Null => J::Null,
        serde_json::Value::Bool(b) => J::Bool(b),
        serde_json::Value::Number(n) => n
            .as_u64()
            .map(J::UInt)
            .or_else(|| n.as_i64().map(J::Int))
            .or_else(|| n.as_f64().map(J::Float))
            .unwrap_or(J::Null),
        serde_json::Value::String(s) => J::String(s),
        serde_json::Value::Array(items) => {
            J::Array(items.into_iter().map(serde_json_to_kernel).collect())
        }
        serde_json::Value::Object(entries) => J::Object(
            entries
                .into_iter()
                .map(|(k, v)| (k, serde_json_to_kernel(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderProfile;
    use starriver_kernel::json::JsonValue;
    use starriver_kernel::llm::{CallId, Message, MessageId, MessageSource, ToolSchema};

    fn profile() -> ProviderProfile {
        ProviderProfile {
            provider: "openai".into(),
            api: "openai-responses".into(),
            base_url: "https://api.example.com".into(),
            credential: None,
            models: Vec::new(),
            retry_policy: None,
            idle_timeout_ms: 300_000,
            max_tokens: Some(4096),
            thinking: None,
            reasoning_effort: None,
        }
    }

    fn options(messages: Vec<Message>) -> GenerateOptions {
        GenerateOptions {
            provider: "openai".into(),
            model: "gpt-5".into(),
            reasoning_effort: None,
            messages,
            system: Some("sys".into()),
            tools: None,
            temperature: None,
            max_tokens: Some(2048),
            stop: None,
            session_id: None,
            agent_id: None,
            tool_choice: None,
            cancellation: None,
            purpose: None,
            loop_assembled: false,
        }
    }

    fn assistant(message: Vec<ContentBlock>) -> Message {
        Message {
            id: MessageId("m1".into()),
            role: MessageRole::Assistant,
            content: message,
            source: MessageSource::Model {
                provider: "openai".into(),
                model: "gpt-5".into(),
                replay_state: None,
            },
        }
    }

    #[test]
    fn serializes_instructions_and_items() {
        let messages = vec![
            Message {
                id: MessageId("m1".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::Text("hi".into())],
                source: MessageSource::User,
            },
            assistant(vec![ContentBlock::tool_call(
                CallId("call_1".into()),
                Some("bash".into()),
                starriver_kernel::json::JsonValue::from_json_str(r#"{"cmd":"ls"}"#).unwrap(),
            )]),
            Message {
                id: MessageId("m3".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: CallId("call_1".into()),
                    content: vec![ContentBlock::Text("file list".into())],
                    is_error: false,
                }],
                source: MessageSource::Tool {
                    call_id: CallId("call_1".into()),
                },
            },
        ];

        let body = serialize_request(&options(messages), &profile()).expect("serialize");
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["max_output_tokens"], 4096);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["content"][0]["text"], "hi");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["arguments"], r#"{"cmd":"ls"}"#);
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["output"], "file list");
    }

    #[test]
    fn reasoning_item_is_replayed_verbatim() {
        let item = serde_json::json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "think"}],
        });
        let messages = vec![assistant(vec![ContentBlock::Custom {
            kind: REASONING_ITEM_KIND.into(),
            payload: serde_json_to_kernel(item.clone()),
        }])];

        let body = serialize_request(&options(messages), &profile()).expect("serialize");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["id"], "rs_1");
        assert_eq!(input[0]["summary"][0]["text"], "think");
    }

    #[test]
    fn stop_is_rejected_as_unsupported() {
        let mut opts = options(Vec::new());
        opts.stop = Some(vec!["</answer>".into()]);
        let err = serialize_request(&opts, &profile()).expect_err("stop must be rejected");
        assert_eq!(err.code(), "UNSUPPORTED_OPTION");
    }

    #[test]
    fn empty_tool_output_gets_placeholder() {
        let messages = vec![Message {
            id: MessageId("m1".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                call_id: CallId("c1".into()),
                content: Vec::new(),
                is_error: false,
            }],
            source: MessageSource::Tool {
                call_id: CallId("c1".into()),
            },
        }];
        let body = serialize_request(&options(messages), &profile()).expect("serialize");
        assert_eq!(body["input"][0]["output"], "(no output)");
    }

    #[test]
    fn image_content_projects_to_input_image_item() {
        let messages = vec![Message {
            id: MessageId("m1".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::Image {
                attachment: JsonValue::Object(vec![(
                    "url".into(),
                    JsonValue::String("data:image/webp;base64,UklGRg==".into()),
                )]),
            }],
            source: MessageSource::User,
        }];
        let body = serialize_request(&options(messages), &profile()).expect("serialize");
        let content = body["input"][0]["content"]
            .as_array()
            .expect("content parts");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "data:image/webp;base64,UklGRg==");
    }

    #[test]
    fn translator_closes_blocks_and_emits_usage() {
        let mut translator = ResponsesTranslator::new();
        let events = [
            r#"{"type":"response.output_item.added","item":{"type":"reasoning","id":"rs_1"}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":" think"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"think"}]}}"#,
            r#"{"type":"response.output_text.delta","delta":"Hello"}"#,
            r#"{"type":"response.output_text.done","text":"Hello"}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"bash"}}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":"{\"cmd\":\"ls\"}"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"bash","arguments":"{\"cmd\":\"ls\"}"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":5,"output_tokens_details":{"reasoning_tokens":3}}}}"#,
        ];
        let mut chunks = Vec::new();
        for event in events {
            chunks.extend(translator.feed(event));
        }
        starriver_llm::contract_tests::assert_finish_is_last(&chunks);
        starriver_llm::contract_tests::assert_open_blocks_are_closed(&chunks);
        assert!(translator.is_done());
        assert!(chunks.iter().any(|c| matches!(
            c,
            Ok(StreamChunk::BlockEnd {
                block: ContentBlock::Custom { kind, .. },
                ..
            }) if kind == REASONING_ITEM_KIND
        )));
        assert!(chunks.iter().any(|c| matches!(
            c,
            Ok(StreamChunk::BlockEnd {
                block: ContentBlock::ToolCall { id, name, .. },
                ..
            }) if id.0 == "call_1" && name.as_deref() == Some("bash")
        )));
        assert!(chunks.iter().any(
            |c| matches!(c, Ok(StreamChunk::Usage { usage }) if usage.reasoning_tokens == Some(3))
        ));
    }

    #[test]
    fn tools_serialize_with_responses_shape() {
        let mut opts = options(Vec::new());
        opts.tools = Some(vec![ToolSchema {
            name: "bash".into(),
            description: "run".into(),
            parameters: JsonValue::Object(vec![(
                "type".into(),
                JsonValue::String("object".into()),
            )]),
        }]);
        let body = serialize_request(&opts, &profile()).expect("serialize");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
    }
}

// ── Round-5 bug regression tests (BUG-48 / BUG-51 / BUG-52 / BUG-53 / BUG-54) ──
#[cfg(test)]
mod round5_tests {
    use super::*;
    use starriver_kernel::llm::{CallId, Message, MessageId, MessageRole};

    fn profile() -> crate::ProviderProfile {
        crate::ProviderProfile {
            provider: "openai".into(),
            api: "openai-responses".into(),
            base_url: "https://api.example.com".into(),
            credential: None,
            models: Vec::new(),
            retry_policy: None,
            idle_timeout_ms: 300_000,
            max_tokens: Some(4096),
            thinking: None,
            reasoning_effort: None,
        }
    }

    fn options(messages: Vec<Message>) -> GenerateOptions {
        GenerateOptions {
            provider: "openai".into(),
            model: "gpt-5".into(),
            reasoning_effort: None,
            messages,
            system: Some("sys".into()),
            tools: None,
            temperature: None,
            max_tokens: Some(2048),
            stop: None,
            session_id: None,
            agent_id: None,
            tool_choice: None,
            cancellation: None,
            purpose: None,
            loop_assembled: false,
        }
    }

    fn user(content: Vec<ContentBlock>) -> Message {
        Message {
            id: MessageId("m1".into()),
            role: MessageRole::User,
            content,
            source: starriver_kernel::llm::MessageSource::User,
        }
    }

    fn image_block(data: &str) -> ContentBlock {
        ContentBlock::Image {
            attachment: starriver_kernel::json::JsonValue::Object(vec![(
                "url".into(),
                starriver_kernel::json::JsonValue::String(format!("data:image/png;base64,{data}")),
            )]),
        }
    }

    /// BUG-48 (responses path): the image of a textless tool-result user turn
    /// must ride the request as an `input_image` part.
    #[test]
    fn user_image_survives_a_textless_tool_result_turn() {
        let messages = vec![user(vec![
            image_block("iVBORw0KGgo="),
            ContentBlock::ToolResult {
                call_id: CallId("c1".into()),
                content: vec![ContentBlock::Text("output".into())],
                is_error: false,
            },
        ])];
        let body = serialize_request(&options(messages), &profile()).expect("serialize");
        let flat = body.to_string();
        assert!(
            flat.contains("input_image") && flat.contains("iVBORw0KGgo"),
            "the image must ride the request even when the turn's text is empty: {flat}"
        );
    }

    /// BUG-52 (responses path): the caller system prompt and System message
    /// parts join with a separator.
    #[test]
    fn instructions_join_with_a_separator() {
        let messages = vec![Message {
            id: MessageId("m1".into()),
            role: MessageRole::System,
            content: vec![ContentBlock::Text("Additional policy.".into())],
            source: starriver_kernel::llm::MessageSource::User,
        }];
        let body =
            serialize_request(&options(messages), &profile()).expect("serialize");
        let instructions = body["instructions"].as_str().unwrap_or_default();
        assert!(
            instructions.contains("sys\n") || instructions.contains("sys\n\n") || instructions.contains("sys "),
            "system segments must not concatenate bare (got {instructions:?})"
        );
    }

    /// BUG-53: a stream that delivers only `response.output_text.done` (no
    /// deltas — proxies do this) must still produce the final text.
    #[test]
    fn text_done_without_delta_still_carries_the_text() {
        let mut t = ResponsesTranslator::new();
        let mut chunks = Vec::new();
        chunks.extend(t.feed(
            r#"{"type":"response.output_text.done","text":"Hello"}"#,
        ));
        chunks.extend(t.feed(
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":3,"output_tokens":2}}}"#,
        ));
        let text = chunks.iter().any(|c| matches!(c,
            Ok(starriver_kernel::llm::StreamChunk::BlockEnd {
                block: starriver_kernel::llm::ContentBlock::Text(t2), ..
            }) if t2 == "Hello"));
        assert!(text, "done-only streams must keep the final text: {chunks:?}");
    }

    /// BUG-47: `response.incomplete` is the Responses API's truncation
    /// terminal — it must map to MaxTokens (kernel drops incomplete tool
    /// calls only under MaxTokens) and keep its usage, not fall through to
    /// a fabricated Stop.
    #[test]
    fn incomplete_terminal_maps_to_max_tokens_with_usage() {
        let mut t = ResponsesTranslator::new();
        let mut chunks = Vec::new();
        chunks.extend(t.feed(
            r#"{"type":"response.output_text.delta","delta":"Hi"}"#,
        ));
        chunks.extend(t.feed(
            r#"{"type":"response.incomplete","response":{"status":"incomplete","usage":{"input_tokens":10,"output_tokens":5}}}"#,
        ));
        let finish = chunks.iter().find_map(|c| match c {
            Ok(starriver_kernel::llm::StreamChunk::Finish { reason, .. }) => Some(reason.clone()),
            _ => None,
        });
        assert_eq!(
            finish,
            Some(starriver_kernel::llm::FinishReason::MaxTokens),
            "incomplete → MaxTokens (got {finish:?}, chunks {chunks:?})"
        );
        assert!(
            chunks.iter().any(|c| matches!(c, Ok(starriver_kernel::llm::StreamChunk::Usage { .. }))),
            "the incomplete terminal carries usage; it must surface"
        );
    }

    /// BUG-99: byte-stream end without a terminal response event = truncated
    /// response — the synthesized finish must be an error, never Stop.
    #[test]
    fn stream_end_without_terminal_event_is_truncation_not_stop() {
        let mut t = ResponsesTranslator::new();
        let mut chunks = Vec::new();
        chunks.extend(t.feed(r#"{"type":"response.output_text.delta","delta":"partial"}"#));
        chunks.extend(t.finish());
        let finish = chunks.iter().find_map(|c| match c {
            Ok(starriver_kernel::llm::StreamChunk::Finish { reason, .. }) => Some(reason.clone()),
            _ => None,
        });
        match finish {
            Some(starriver_kernel::llm::FinishReason::Error { failure }) => {
                assert_eq!(failure.code, "PROVIDER_STREAM_TRUNCATED");
            }
            other => panic!("truncated stream must surface an error, got {other:?}"),
        }
    }

    /// BUG-54: the profile's reasoning_effort is advertised by resolve_model
    /// (lib.rs) — the responses request must carry it, not silently drop it.
    #[test]
    fn reasoning_effort_rides_the_request() {
        let mut opts = options(vec![user(vec![ContentBlock::Text("hi".into())])]);
        opts.reasoning_effort = Some("high".into());
        let body = serialize_request(&opts, &profile()).expect("serialize");
        assert!(
            body.get("reasoning").is_some(),
            "reasoning effort must map to the responses `reasoning` field: {body}"
        );
    }

    /// BUG-51: same crate, two routers, one token vocabulary — responses
    /// `input_tokens` includes cached tokens exactly like completions
    /// `prompt_tokens` does; the net input must subtract the cache read or
    /// the meter double-counts every cached token.
    #[test]
    fn map_usage_subtracts_cached_tokens_from_input() {
        let usage = serde_json::json!({
            "input_tokens": 100,
            "input_tokens_details": { "cached_tokens": 90 },
            "output_tokens": 5,
        });
        let mapped = map_usage(&usage);
        assert_eq!(mapped.input_tokens, 10);
        assert_eq!(mapped.cache_read_tokens, Some(90));
    }
}
