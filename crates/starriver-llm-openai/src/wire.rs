//! Request serialization + HTTP status/error normalization.
//!
//! The serializer mirrors `llm-deepseek/src/serialize.ts` path: every
//! `tool-result` block in a user message becomes its own `role:"tool"`
//! message, assistant reasoning is passed back via `reasoning_content`,
//! assistant `content` is never `null`, empty tool output gets a `(no output)`
//! placeholder, and image content becomes `image_url` parts (attachment
//! admission → inline data-URL projection).

use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::{CallPurpose, ContentBlock, GenerateOptions, LlmError, MessageRole};

use crate::ProviderProfile;

const NO_OUTPUT_PLACEHOLDER: &str = "(no output)";

pub fn serialize_request(
    options: &GenerateOptions,
    profile: &ProviderProfile,
) -> Result<serde_json::Value, LlmError> {
    let mut messages = Vec::new();
    if let Some(system) = &options.system {
        messages.push(serde_json::json!({"role": "system", "content": system}));
    }
    for message in &options.messages {
        match message.role {
            MessageRole::System => {
                messages.push(serde_json::json!({
                    "role": "system",
                    "content": flatten_text(&message.content),
                }));
            }
            MessageRole::User => {
 // `serializeMessages`: harness user messages may carry text
                // and tool results together; text stays on the user message and
                // each tool result is expanded into its own `role:"tool"`
                // message. Image blocks project to `image_url` parts on the
                // user message (inline data URLs from the attachment seam).
                let text = flatten_text(&message.content);
                let images = collect_image_urls(&message.content)?;
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
                // BUG-48: images alone still carry the turn — a textless
                // tool-result turn must not drop the user message (and its
                // images) from the request.
                if !text.is_empty() || tool_results.is_empty() || !images.is_empty() {
                    let content: serde_json::Value = if images.is_empty() {
                        serde_json::json!(text)
                    } else {
                        let mut parts = vec![serde_json::json!({
                            "type": "text",
                            "text": text,
                        })];
                        for url in images {
                            parts.push(serde_json::json!({
                                "type": "image_url",
                                "image_url": { "url": url },
                            }));
                        }
                        serde_json::json!(parts)
                    };
                    messages.push(serde_json::json!({"role": "user", "content": content}));
                }
                for (call_id, content) in tool_results {
                    let output = flatten_text(content);
                    messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": call_id.0.clone(),
                        "content": if output.is_empty() {
                            NO_OUTPUT_PLACEHOLDER.to_string()
                        } else {
                            output
                        },
                    }));
                }
            }
            MessageRole::Assistant => {
 // `serializeAssistant`: content is "" — NEVER null; pure
                // tool-call turns replay `content: ""` (some gateways reject
                // null outright), and reasoning is passed back verbatim on
                // every reasoning-carrying turn (required for thinking mode).
                let text = flatten_text(&message.content);
                let reasoning: String = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Reasoning(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect();
                let calls: Vec<serde_json::Value> = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some(serde_json::json!({
                            "id": id.0.clone(),
                            "type": "function",
                            "function": {
                                "name": name.clone().unwrap_or_default(),
                                // OpenAI's wire format requires string
                                // arguments — the one sanctioned
                                // serialization at this boundary.
                                "arguments": arguments
                                    .to_json_string()
                                    .unwrap_or_default(),
                            },
                        })),
                        _ => None,
                    })
                    .collect();
                let mut entry = serde_json::json!({"role": "assistant", "content": text});
                if !reasoning.is_empty() {
                    entry["reasoning_content"] = serde_json::json!(reasoning);
                }
                if !calls.is_empty() {
                    entry["tool_calls"] = serde_json::json!(calls);
                }
                messages.push(entry);
            }
        }
    }

    let mut body = serde_json::json!({
        "model": options.model,
        "messages": messages,
        "stream": true,
 // Usage reporting on by default, matching `requestWithMessages`.
        "stream_options": { "include_usage": true },
    });

    let (thinking, reasoning_effort) = resolve_thinking(options, profile)?;
    if let Some(thinking) = thinking {
        body["thinking"] = serde_json::json!({"type": thinking});
    }
    if let Some(effort) = reasoning_effort {
        body["reasoning_effort"] = serde_json::json!(effort);
    }

    if let Some(max_tokens) = profile.max_tokens.or(options.max_tokens) {
        body["max_tokens"] = serde_json::json!(max_tokens);
    }
    if let Some(temperature) = options.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    if let Some(tools) = &options.tools {
        if !tools.is_empty() {
            let wire_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|schema| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": schema.name,
                            "description": schema.description,
                            "parameters": json_to_serde(&schema.parameters),
                        },
                    })
                })
                .collect();
            body["tools"] = serde_json::json!(wire_tools);
        }
    }
    if let Some(stop) = &options.stop {
        if !stop.is_empty() {
            body["stop"] = serde_json::json!(stop);
        }
    }
    Ok(body)
}

/// Collect the image URLs of every `Image` block in a block list, in order.
/// The block's `attachment` payload must carry a string `url` (inline data URL
/// or an http(s) URL); a malformed payload fails loudly rather than silently
/// dropping the image (`INVALID_ATTACHMENT_REF`).
pub(crate) fn collect_image_urls(blocks: &[ContentBlock]) -> Result<Vec<String>, LlmError> {
    let mut urls = Vec::new();
    for block in blocks {
        let ContentBlock::Image { attachment } = block else {
            continue;
        };
        let url = match attachment {
            JsonValue::Object(entries) => entries
                .iter()
                .find(|(key, _)| key == "url")
                .and_then(|(_, value)| value.as_str())
                .map(str::to_string),
            _ => None,
        };
        match url {
            Some(url) if !url.is_empty() => urls.push(url),
            _ => {
                return Err(LlmError::Call {
                    code: "INVALID_ATTACHMENT_REF".into(),
                    message: "image block attachment must carry a non-empty `url`".into(),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                })
            }
        }
    }
    Ok(urls)
}

/// Resolve adapter-level thinking mode and reasoning effort, mirroring
/// `resolveThinking`: session-title calls force thinking off; an explicit or
/// profile-default effort drives the wire fields ("off" → thinking disabled,
/// low/high/max → thinking enabled + `reasoning_effort`); unsupported efforts
/// are rejected.
fn resolve_thinking(
    options: &GenerateOptions,
    profile: &ProviderProfile,
) -> Result<(Option<String>, Option<String>), LlmError> {
    if options.purpose == Some(CallPurpose::SessionTitle) {
        return Ok((Some("disabled".into()), None));
    }
    let effort = options
        .reasoning_effort
        .as_deref()
        .or(profile.reasoning_effort.as_deref());
    if let Some(effort) = effort {
        if !matches!(effort, "off" | "low" | "high" | "max") {
            return Err(unsupported_reasoning_effort(effort));
        }
        if profile.thinking.as_deref() == Some("disabled") && effort != "off" {
            return Err(unsupported_reasoning_effort(effort));
        }
    }
    Ok(match effort {
        Some("off") => (Some("disabled".into()), None),
        Some(effort @ ("low" | "high" | "max")) => (Some("enabled".into()), Some(effort.into())),
        _ => {
            let thinking = match profile.thinking.as_deref() {
                Some(thinking @ ("enabled" | "disabled")) => Some(thinking.to_string()),
                Some(other) => {
                    return Err(LlmError::Call {
                        code: "INVALID_REQUEST".into(),
                        message: format!(
                            "provider profile thinking `{other}` must be enabled or disabled"
                        ),
                        status: None,
                        provider_retry_after_ms: None,
                        request_id: None,
                    });
                }
                None => None,
            };
            (thinking, None)
        }
    })
}

fn unsupported_reasoning_effort(effort: &str) -> LlmError {
    LlmError::Call {
        code: "UNSUPPORTED_REASONING_EFFORT".into(),
        message: format!("DeepSeek does not support reasoning effort \"{effort}\""),
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    }
}

/// Concatenate the text blocks of a block list (text-only vocabulary).
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

pub fn parse_retry_after(header: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    let value = header?.to_str().ok()?;
    let seconds: u64 = value.trim().parse().ok()?;
    Some(seconds * 1_000)
}

/// Pull the provider request id from the conventional response headers,
/// mirroring requestId propagation.
pub fn parse_request_id(headers: &reqwest::header::HeaderMap) -> Option<String> {
    const NAMES: [&str; 3] = ["x-request-id", "x-amzn-requestid", "x-amz-request-id"];
    for name in NAMES {
        let value = headers.get(name)?.to_str().ok()?.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

pub fn normalize_status(status: u16, body: &str) -> &'static str {
    let m = body.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| m.contains(n));
    match status {
        401 | 403 => "AUTH",
        413 => "INVALID_REQUEST",
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

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::json::JsonValue;
    use starriver_kernel::llm::{
        CallId, CallPurpose, ContentBlock, GenerateOptions, Message, MessageId, MessageRole,
        MessageSource, ToolSchema,
    };

    /// BUG-102: a 429 announcing billing/quota exhaustion is QUOTA_EXHAUSTED
    /// (non-retryable), not RATE_LIMIT.
    #[test]
    fn quota_exhaustion_429_is_not_rate_limit() {
        assert_eq!(
            normalize_status(429, r#"{"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}"#),
            "QUOTA_EXHAUSTED"
        );
        assert_eq!(normalize_status(429, "insufficient balance"), "QUOTA_EXHAUSTED");
        // A genuine rate limit stays RATE_LIMIT.
        assert_eq!(normalize_status(429, "too many requests"), "RATE_LIMIT");
    }

    fn profile() -> ProviderProfile {
        ProviderProfile {
            provider: "deepseek".into(),
            api: "openai-completions".into(),
            base_url: "https://api.example.com".into(),
            credential: None,
            models: Vec::new(),
            retry_policy: None,
            idle_timeout_ms: 300_000,
            max_tokens: None,
            thinking: None,
            reasoning_effort: None,
        }
    }

    fn options(messages: Vec<Message>) -> GenerateOptions {
        GenerateOptions {
            provider: "deepseek".into(),
            model: "deepseek-v4-flash".into(),
            reasoning_effort: None,
            messages,
            system: Some("sys".into()),
            tools: None,
            temperature: None,
            max_tokens: Some(8192),
            stop: None,
            session_id: None,
            agent_id: None,
            tool_choice: None,
            cancellation: None,
            purpose: None,
            loop_assembled: true,
        }
    }

    fn assistant_tool_call(call_id: &str, name: &str, arguments: &str) -> Message {
        Message {
            id: MessageId("m1".into()),
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: CallId(call_id.into()),
                name: Some(name.into()),
                arguments: starriver_kernel::json::JsonValue::from_json_str(arguments)
                    .unwrap_or(starriver_kernel::json::JsonValue::String(arguments.into())),
            }],
            source: MessageSource::Model {
                provider: "deepseek".into(),
                model: "deepseek-v4-flash".into(),
                replay_state: None,
            },
        }
    }

    fn user_tool_result(call_id: &str, text: &str) -> Message {
        Message {
            id: MessageId("m2".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                call_id: CallId(call_id.into()),
                content: vec![ContentBlock::Text(text.into())],
                is_error: false,
            }],
            source: MessageSource::Tool {
                call_id: CallId(call_id.into()),
            },
        }
    }

    #[test]
    fn serializes_tool_definitions_into_the_wire_body() {
        let mut opts = options(Vec::new());
        opts.tools = Some(vec![ToolSchema {
            name: "bash".into(),
            description: "Run a shell command".into(),
            parameters: JsonValue::Object(vec![(
                "type".into(),
                JsonValue::String("object".into()),
            )]),
        }]);

        let body = serialize_request(&opts, &profile()).expect("serialize");
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "bash");
        assert_eq!(tools[0]["function"]["description"], "Run a shell command");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["max_tokens"], 8192);
    }

    #[test]
    fn serializes_assistant_tool_calls_and_tool_results() {
        let messages = vec![
            assistant_tool_call("call_1", "bash", r#"{"command":"ls"}"#),
            user_tool_result("call_1", "file list"),
        ];

        let body = serialize_request(&options(messages), &profile()).expect("serialize");
        let wire = body["messages"].as_array().expect("messages array");
        assert_eq!(wire[0]["role"], "system");
        let assistant = &wire[1];
        assert_eq!(assistant["role"], "assistant");
 //: text-less tool-call turns send "" — NEVER null.
        assert_eq!(assistant["content"], "");
        assert!(assistant.get("reasoning_content").is_none());
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            r#"{"command":"ls"}"#
        );
        let tool_result = &wire[2];
        assert_eq!(tool_result["role"], "tool");
        assert_eq!(tool_result["tool_call_id"], "call_1");
        assert_eq!(tool_result["content"], "file list");
    }

    #[test]
    fn assistant_reasoning_is_passed_back_on_the_wire() {
        let mut message = assistant_tool_call("call_1", "bash", r#"{"command":"ls"}"#);
        message
            .content
            .insert(0, ContentBlock::Reasoning("think step by step".into()));
        let messages = vec![message, user_tool_result("call_1", "file list")];

        let body = serialize_request(&options(messages), &profile()).expect("serialize");
        let wire = body["messages"].as_array().expect("messages array");
        let assistant = &wire[1];
        assert_eq!(assistant["content"], "");
        assert_eq!(assistant["reasoning_content"], "think step by step");
        assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn empty_tool_output_gets_placeholder() {
        let message = Message {
            id: MessageId("m2".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                call_id: CallId("call_1".into()),
                content: Vec::new(),
                is_error: false,
            }],
            source: MessageSource::Tool {
                call_id: CallId("call_1".into()),
            },
        };
        let messages = vec![assistant_tool_call("call_1", "bash", "{}"), message];

        let body = serialize_request(&options(messages), &profile()).expect("serialize");
        let wire = body["messages"].as_array().expect("messages array");
        assert_eq!(wire[2]["role"], "tool");
        assert_eq!(wire[2]["content"], "(no output)");
    }

    #[test]
    fn mixed_user_message_expands_text_and_each_tool_result() {
        let mut message = user_tool_result("call_1", "");
        message.content = vec![
            ContentBlock::Text("note before results".into()),
            ContentBlock::ToolResult {
                call_id: CallId("call_1".into()),
                content: vec![ContentBlock::Text("first".into())],
                is_error: false,
            },
            ContentBlock::ToolResult {
                call_id: CallId("call_2".into()),
                content: vec![ContentBlock::Text("second".into())],
                is_error: false,
            },
        ];
        let messages = vec![message];

        let body = serialize_request(&options(messages), &profile()).expect("serialize");
        let wire = body["messages"].as_array().expect("messages array");
        assert_eq!(wire[0]["role"], "system");
        assert_eq!(wire[1]["role"], "user");
        assert_eq!(wire[1]["content"], "note before results");
        assert_eq!(wire[2]["role"], "tool");
        assert_eq!(wire[2]["tool_call_id"], "call_1");
        assert_eq!(wire[2]["content"], "first");
        assert_eq!(wire[3]["role"], "tool");
        assert_eq!(wire[3]["tool_call_id"], "call_2");
        assert_eq!(wire[3]["content"], "second");
    }

    #[test]
    fn image_content_projects_to_inline_image_url_parts() {
        let message = Message {
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
        };

        let body = serialize_request(&options(vec![message]), &profile()).expect("serialize");
        let user = &body["messages"][1];
        assert_eq!(user["role"], "user");
        let parts = user["content"].as_array().expect("content parts");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "describe this");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(
            parts[1]["image_url"]["url"],
            "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg=="
        );
    }

    #[test]
    fn malformed_image_attachment_fails_loudly() {
        let message = Message {
            id: MessageId("m1".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::Image {
                attachment: JsonValue::Object(vec![(
                    "ref".into(),
                    JsonValue::String("sha256:abc".into()),
                )]),
            }],
            source: MessageSource::User,
        };
        let err = serialize_request(&options(vec![message]), &profile())
            .expect_err("attachment without url must fail");
        assert_eq!(err.code(), "INVALID_ATTACHMENT_REF");
    }

    #[test]
    fn stop_sequences_are_serialized() {
        let mut opts = options(Vec::new());
        opts.stop = Some(vec!["</answer>".into(), "done".into()]);

        let body = serialize_request(&opts, &profile()).expect("serialize");
        assert_eq!(body["stop"], serde_json::json!(["</answer>", "done"]));
    }

    #[test]
    fn explicit_reasoning_effort_maps_to_thinking_and_effort() {
        let mut opts = options(Vec::new());
        opts.reasoning_effort = Some("high".into());

        let body = serialize_request(&opts, &profile()).expect("serialize");
        assert_eq!(body["thinking"], serde_json::json!({"type": "enabled"}));
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn off_effort_disables_thinking_without_effort_field() {
        let mut opts = options(Vec::new());
        opts.reasoning_effort = Some("off".into());

        let body = serialize_request(&opts, &profile()).expect("serialize");
        assert_eq!(body["thinking"], serde_json::json!({"type": "disabled"}));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn profile_effort_default_applies_when_options_omit_it() {
        let mut p = profile();
        p.reasoning_effort = Some("max".into());

        let body = serialize_request(&options(Vec::new()), &p).expect("serialize");
        assert_eq!(body["thinking"], serde_json::json!({"type": "enabled"}));
        assert_eq!(body["reasoning_effort"], "max");
    }

    #[test]
    fn invalid_effort_and_disabled_thinking_conflicts_are_rejected() {
        let mut opts = options(Vec::new());
        opts.reasoning_effort = Some("extreme".into());
        let err = serialize_request(&opts, &profile()).expect_err("invalid effort");
        assert_eq!(err.code(), "UNSUPPORTED_REASONING_EFFORT");

        let mut p = profile();
        p.thinking = Some("disabled".into());
        opts.reasoning_effort = Some("high".into());
        let err = serialize_request(&opts, &p).expect_err("conflict");
        assert_eq!(err.code(), "UNSUPPORTED_REASONING_EFFORT");
    }

    #[test]
    fn session_title_calls_force_thinking_disabled() {
        let mut opts = options(Vec::new());
        opts.purpose = Some(CallPurpose::SessionTitle);
        opts.reasoning_effort = Some("high".into());

        let body = serialize_request(&opts, &profile()).expect("serialize");
        assert_eq!(body["thinking"], serde_json::json!({"type": "disabled"}));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn parse_request_id_reads_common_provider_headers() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_static("x-request-id"),
            reqwest::header::HeaderValue::from_static("req_abc"),
        );
        assert_eq!(parse_request_id(&headers).as_deref(), Some("req_abc"));

        let empty = reqwest::header::HeaderMap::new();
        assert_eq!(parse_request_id(&empty), None);
    }
}

// ── Round-5 bug regression tests (BUG-48 / BUG-51 / BUG-52) ──
#[cfg(test)]
mod round5_tests {
    use super::*;
    use starriver_kernel::json::JsonValue;
    use starriver_kernel::llm::{
        CallId, ContentBlock, Message, MessageId, MessageRole, MessageSource,
    };

    fn profile() -> ProviderProfile {
        ProviderProfile {
            provider: "deepseek".into(),
            api: "openai-completions".into(),
            base_url: "https://api.example.com".into(),
            credential: None,
            models: Vec::new(),
            retry_policy: None,
            idle_timeout_ms: 300_000,
            max_tokens: None,
            thinking: None,
            reasoning_effort: None,
        }
    }

    fn options(messages: Vec<Message>) -> starriver_kernel::llm::GenerateOptions {
        starriver_kernel::llm::GenerateOptions {
            provider: "deepseek".into(),
            model: "deepseek-v4-flash".into(),
            reasoning_effort: None,
            messages,
            system: Some("sys".into()),
            tools: None,
            temperature: None,
            max_tokens: Some(8192),
            stop: None,
            session_id: None,
            agent_id: None,
            tool_choice: None,
            cancellation: None,
            purpose: None,
            loop_assembled: true,
        }
    }

    fn image_block(data: &str) -> ContentBlock {
        ContentBlock::Image {
            attachment: JsonValue::Object(vec![(
                "url".into(),
                JsonValue::String(format!("data:image/png;base64,{data}")),
            )]),
        }
    }

    fn user_message(content: Vec<ContentBlock>) -> Message {
        Message {
            id: MessageId("m1".into()),
            role: MessageRole::User,
            content,
            source: MessageSource::User,
        }
    }

    /// BUG-48: a user turn with no text but a tool result AND an image must
    /// still deliver the image — the `!text.is_empty() || tool_results.is_empty()`
    /// guard dropped the whole user message (module doc: "Image blocks project
    /// to `image_url` parts on the user message").
    #[test]
    fn user_image_survives_a_textless_tool_result_turn() {
        let messages = vec![user_message(vec![
            image_block("iVBORw0KGgo="),
            ContentBlock::ToolResult {
                call_id: CallId("c1".into()),
                content: vec![ContentBlock::Text("output".into())],
                is_error: false,
            },
        ])];
        let body = serialize_request(&options(messages), &profile()).expect("serialize");
        let wire: Vec<&serde_json::Value> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .collect();
        let image_part = wire.iter().any(|m| {
            m["content"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .any(|p| p["type"] == "image_url" && p["image_url"]["url"].as_str().is_some_and(|u| u.contains("iVBOR")))
                })
                .unwrap_or(false)
        });
        assert!(
            image_part,
            "the image must ride the user message even when the turn's text is empty: {body}"
        );
    }

    /// BUG-52: multiple text blocks in one message join with a separator —
    /// bare concatenation corrupts content ("note"+"go on" → "notego on").
    #[test]
    fn multi_block_text_joins_with_separator() {
        let messages = vec![user_message(vec![
            ContentBlock::Text("note".into()),
            ContentBlock::ToolResult {
                call_id: CallId("c1".into()),
                content: vec![ContentBlock::Text("out".into())],
                is_error: false,
            },
            ContentBlock::Text("go on".into()),
        ])];
        let body = serialize_request(&options(messages), &profile()).expect("serialize");
        let user = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "user")
            .unwrap();
        let text = user["content"].as_str().unwrap_or_default();
        assert!(
            text.contains("note\n\ngo on") || text.contains("note\ngo on") || text.contains("note go on"),
            "text blocks must not concatenate bare (got {text:?})"
        );
    }
}
