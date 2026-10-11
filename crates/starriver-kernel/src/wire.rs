//! Wire mapping between the LLM vocabulary and the lossless `JsonValue`
//! tree. Known variants use fixed tags (`text`, `tool-call`, ...); plugin
//! extensions ride `Custom` variants as `{ type/kind, ...payload }`.

use crate::json::{JsonValue, JsonValueError};
use crate::llm::{
    CallId, ContentBlock, ContentBlockType, ContextForm, ContextSnapshotSection, FinishReason,
    LlmFailure, Message, MessageId, MessageRole, MessageSource, ReplayEnvelope, StreamChunk,
    TokenUsage,
};

impl ContentBlock {
    pub fn to_json(&self) -> Result<JsonValue, JsonValueError> {
        let mut entries = Vec::new();
        let tag = match self {
            ContentBlock::Text(text) => {
                entries.push(("text".into(), JsonValue::String(text.clone())));
                "text"
            }
            ContentBlock::Reasoning(text) => {
                entries.push(("text".into(), JsonValue::String(text.clone())));
                "reasoning"
            }
            ContentBlock::Image { attachment } => {
                entries.push(("attachment".into(), attachment.clone()));
                "image"
            }
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => {
                entries.push(("id".into(), JsonValue::String(id.0.clone())));
                entries.push((
                    "name".into(),
                    name.clone()
                        .map(JsonValue::String)
                        .unwrap_or(JsonValue::Null),
                ));
                entries.push(("arguments".into(), arguments.clone()));
                "tool-call"
            }
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                entries.push(("toolCallId".into(), JsonValue::String(call_id.0.clone())));
                entries.push((
                    "content".into(),
                    JsonValue::Array(
                        content
                            .iter()
                            .map(ContentBlock::to_json)
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                ));
                entries.push(("isError".into(), JsonValue::Bool(*is_error)));
                "tool-result"
            }
            ContentBlock::Custom { kind, payload } => {
                // Reserved tags would be parsed back as the BUILT-IN
                // variant (the Custom identity and payload are lost) —
                // reject at emission so the bug is loud, not silent.
                if matches!(
                    kind.as_str(),
                    "text" | "reasoning" | "image" | "tool-call" | "tool-result"
                ) {
                    return Err(JsonValueError::DuplicateTagKey(kind.clone()));
                }
                return JsonValue::with_tag("type", kind, payload.clone());
            }
        };
        entries.insert(0, ("type".into(), JsonValue::String(tag.into())));
        Ok(JsonValue::Object(entries))
    }

    pub fn from_json(value: JsonValue) -> Result<ContentBlock, JsonValueError> {
        let (tag, fields) = value.split_tag("type").ok_or(JsonValueError::NotObject)?;
        match tag.as_str() {
            "text" => Ok(ContentBlock::Text(
                fields
                    .get("text")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )),
            "reasoning" => Ok(ContentBlock::Reasoning(
                fields
                    .get("text")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )),
            "image" => Ok(ContentBlock::Image {
                attachment: fields.get("attachment").cloned().unwrap_or(JsonValue::Null),
            }),
            "tool-call" => Ok(ContentBlock::ToolCall {
                id: CallId(
                    fields
                        .get("id")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
                name: fields
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .map(String::from),
                // WI-47 Phase B dual-format replay: old session logs store
                // `arguments` as a string (parse once through the shared
                // ingress rule); new logs store the lossless object
                // directly.
                arguments: match fields.get("arguments") {
                    Some(JsonValue::String(text)) => {
                        crate::llm::tool_call_arguments_from_text(text)
                    }
                    Some(other) => other.clone(),
                    None => JsonValue::Object(Vec::new()),
                },
            }),
            "tool-result" => {
                let content = fields
                    .get("content")
                    .and_then(JsonValue::as_array)
                    .unwrap_or(&[])
                    .iter()
                    .cloned()
                    .map(ContentBlock::from_json)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(ContentBlock::ToolResult {
                    call_id: CallId(
                        fields
                            .get("toolCallId")
                            .and_then(JsonValue::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    ),
                    content,
                    is_error: fields
                        .get("isError")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false),
                })
            }
            other => Ok(ContentBlock::Custom {
                kind: other.to_string(),
                payload: fields,
            }),
        }
    }
}

impl ContextForm {
    pub fn to_json(&self) -> Result<JsonValue, JsonValueError> {
        let mut entries = Vec::new();
        let tag = match self {
            ContextForm::Instructions => "instructions",
            ContextForm::Catalog => "catalog",
            ContextForm::Relay => "relay",
            ContextForm::Recall => "recall",
            ContextForm::Snapshot {
                sections,
                truncated,
                budget_bytes,
                composed_bytes,
            } => {
                entries.push((
                    "sections".into(),
                    JsonValue::Array(
                        sections
                            .iter()
                            .map(|section| {
                                Ok(JsonValue::Object(vec![
                                    ("name".into(), JsonValue::String(section.name.clone())),
                                    ("text".into(), JsonValue::String(section.text.clone())),
                                ]))
                            })
                            .collect::<Result<Vec<_>, JsonValueError>>()?,
                    ),
                ));
                if *truncated {
                    entries.push(("truncated".into(), JsonValue::Bool(true)));
                }
                if *budget_bytes > 0 {
                    entries.push(("budgetBytes".into(), JsonValue::UInt(*budget_bytes)));
                }
                if *composed_bytes > 0 {
                    entries.push(("composedBytes".into(), JsonValue::UInt(*composed_bytes)));
                }
                "snapshot"
            }
            ContextForm::Notice { summary } => {
                entries.push(("summary".into(), JsonValue::String(summary.clone())));
                "notice"
            }
        };
        entries.insert(0, ("form".into(), JsonValue::String(tag.into())));
        Ok(JsonValue::Object(entries))
    }

    pub fn from_json(value: JsonValue) -> Result<ContextForm, JsonValueError> {
        let tag = value.as_str_at("form").ok_or(JsonValueError::NotObject)?;
        match tag {
            "instructions" => Ok(ContextForm::Instructions),
            "catalog" => Ok(ContextForm::Catalog),
            "relay" => Ok(ContextForm::Relay),
            "recall" => Ok(ContextForm::Recall),
            "snapshot" => {
                let sections = value
                    .get("sections")
                    .and_then(JsonValue::as_array)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|section| {
                        Some(ContextSnapshotSection {
                            name: section.get("name")?.as_str()?.to_string(),
                            text: section.get("text")?.as_str()?.to_string(),
                        })
                    })
                    .collect();
                Ok(ContextForm::Snapshot {
                    sections,
                    truncated: value
                        .get("truncated")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false),
                    budget_bytes: value
                        .get("budgetBytes")
                        .and_then(JsonValue::as_u64)
                        .unwrap_or(0),
                    composed_bytes: value
                        .get("composedBytes")
                        .and_then(JsonValue::as_u64)
                        .unwrap_or(0),
                })
            }
            "notice" => Ok(ContextForm::Notice {
                summary: value
                    .get("summary")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            _ => Err(JsonValueError::NotObject),
        }
    }
}

impl MessageSource {
    pub fn to_json(&self) -> Result<JsonValue, JsonValueError> {
        let mut entries = Vec::new();
        let tag = match self {
            MessageSource::User => "user",
            MessageSource::Plugin { plugin, form } => {
                entries.push(("plugin".into(), JsonValue::String(plugin.clone())));
                if let Some(form) = form {
                    entries.push(("form".into(), form.to_json()?));
                }
                "plugin"
            }
            MessageSource::Model {
                provider,
                model,
                replay_state,
            } => {
                entries.push(("provider".into(), JsonValue::String(provider.clone())));
                entries.push(("model".into(), JsonValue::String(model.clone())));
                if let Some(replay_state) = replay_state {
                    entries.push(("replayState".into(), replay_state.clone()));
                }
                "model"
            }
            MessageSource::Tool { call_id } => {
                entries.push(("callId".into(), JsonValue::String(call_id.0.clone())));
                "tool"
            }
            MessageSource::Custom { kind, payload } => {
                // Reserved kinds would parse back as the BUILT-IN variant,
                // silently dropping the Custom identity/payload.
                if matches!(
                    kind.as_str(),
                    "user" | "assistant" | "tool" | "plugin" | "model" | "system"
                ) {
                    return Err(JsonValueError::DuplicateTagKey(kind.clone()));
                }
                return JsonValue::with_tag("kind", kind, payload.clone());
            }
        };
        entries.insert(0, ("kind".into(), JsonValue::String(tag.into())));
        Ok(JsonValue::Object(entries))
    }

    pub fn from_json(value: JsonValue) -> Result<MessageSource, JsonValueError> {
        let (tag, fields) = value.split_tag("kind").ok_or(JsonValueError::NotObject)?;
        match tag.as_str() {
            "user" => Ok(MessageSource::User),
            "plugin" => Ok(MessageSource::Plugin {
                plugin: fields
                    .get("plugin")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
                form: fields
                    .get("form")
                    .cloned()
                    .map(ContextForm::from_json)
                    .transpose()?,
            }),
            "model" => Ok(MessageSource::Model {
                provider: fields
                    .get("provider")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
                model: fields
                    .get("model")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
                replay_state: fields.get("replayState").cloned(),
            }),
            "tool" => Ok(MessageSource::Tool {
                call_id: CallId(
                    fields
                        .get("callId")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
            }),
            other => Ok(MessageSource::Custom {
                kind: other.to_string(),
                payload: fields,
            }),
        }
    }
}

impl Message {
    /// Wire shape: `{ id, role, content: ContentBlock[], source }`.
    pub fn to_json(&self) -> Result<JsonValue, JsonValueError> {
        let role = match self.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        };
        Ok(JsonValue::Object(vec![
            ("id".into(), JsonValue::String(self.id.0.clone())),
            ("role".into(), JsonValue::String(role.into())),
            (
                "content".into(),
                JsonValue::Array(
                    self.content
                        .iter()
                        .map(ContentBlock::to_json)
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            ),
            ("source".into(), self.source.to_json()?),
        ]))
    }

    pub fn from_json(value: JsonValue) -> Result<Message, JsonValueError> {
        let role = match value.as_str_at("role") {
            Some("system") => MessageRole::System,
            Some("user") => MessageRole::User,
            Some("assistant") => MessageRole::Assistant,
            _ => return Err(JsonValueError::NotObject),
        };
        Ok(Message {
            id: MessageId(
                value
                    .get("id")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
            role,
            content: value
                .get("content")
                .and_then(JsonValue::as_array)
                .unwrap_or(&[])
                .iter()
                .cloned()
                .map(ContentBlock::from_json)
                .collect::<Result<Vec<_>, _>>()?,
            source: MessageSource::from_json(
                value.get("source").cloned().unwrap_or(JsonValue::Null),
            )?,
        })
    }
}

impl ContentBlockType {
    /// Stable stream-protocol tag; custom kinds use their own string.
    pub(crate) fn tag(&self) -> String {
        match self {
            ContentBlockType::Text => "text".into(),
            ContentBlockType::Reasoning => "reasoning".into(),
            ContentBlockType::Image => "image".into(),
            ContentBlockType::ToolCall => "tool-call".into(),
            ContentBlockType::ToolResult => "tool-result".into(),
            ContentBlockType::Custom(kind) => kind.clone(),
        }
    }

    pub(crate) fn from_tag(tag: &str) -> ContentBlockType {
        match tag {
            "text" => ContentBlockType::Text,
            "reasoning" => ContentBlockType::Reasoning,
            "image" => ContentBlockType::Image,
            "tool-call" => ContentBlockType::ToolCall,
            "tool-result" => ContentBlockType::ToolResult,
            other => ContentBlockType::Custom(other.into()),
        }
    }
}

impl StreamChunk {
    /// Wire shape `{ type, ...payload }`; block-start carries `blockType` to
    /// avoid colliding with the envelope's `type` tag.
    pub fn to_json(&self) -> Result<JsonValue, JsonValueError> {
        let (tag, entries) = match self {
            StreamChunk::BlockStart { index, block_type } => (
                "block-start",
                vec![
                    ("index".into(), JsonValue::UInt(*index)),
                    ("blockType".into(), JsonValue::String(block_type.tag())),
                ],
            ),
            StreamChunk::TextDelta { index, text } => (
                "text-delta",
                vec![
                    ("index".into(), JsonValue::UInt(*index)),
                    ("text".into(), JsonValue::String(text.clone())),
                ],
            ),
            StreamChunk::ReasoningDelta { index, text } => (
                "reasoning-delta",
                vec![
                    ("index".into(), JsonValue::UInt(*index)),
                    ("text".into(), JsonValue::String(text.clone())),
                ],
            ),
            StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => (
                "tool-call-delta",
                vec![
                    ("index".into(), JsonValue::UInt(*index)),
                    ("id".into(), JsonValue::String(id.0.clone())),
                    (
                        "name".into(),
                        name.clone()
                            .map(JsonValue::String)
                            .unwrap_or(JsonValue::Null),
                    ),
                    (
                        "argumentsDelta".into(),
                        JsonValue::String(arguments_delta.clone()),
                    ),
                ],
            ),
            StreamChunk::BlockEnd { index, block } => (
                "block-end",
                vec![
                    ("index".into(), JsonValue::UInt(*index)),
                    ("block".into(), block.to_json()?),
                ],
            ),
            StreamChunk::Usage { usage } => (
                "usage",
                vec![
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
                ],
            ),
            StreamChunk::Finish {
                reason,
                replay_state,
            } => {
                let mut entries = vec![("reason".into(), reason.to_json()?)];
                if let Some(replay_state) = replay_state {
                    entries.push((
                        "replayState".into(),
                        JsonValue::Object(vec![
                            ("response".into(), replay_state.response.clone()),
                            (
                                "blocks".into(),
                                replay_state
                                    .blocks
                                    .clone()
                                    .map(JsonValue::Array)
                                    .unwrap_or(JsonValue::Null),
                            ),
                        ]),
                    ));
                }
                ("finish", entries)
            }
        };
        let mut entries = entries;
        entries.insert(0, ("type".into(), JsonValue::String(tag.into())));
        Ok(JsonValue::Object(entries))
    }

    pub fn from_json(value: JsonValue) -> Result<StreamChunk, JsonValueError> {
        let tag = value.as_str_at("type").ok_or(JsonValueError::NotObject)?;
        let index = value.get("index").and_then(JsonValue::as_u64).unwrap_or(0);
        match tag {
            "block-start" => Ok(StreamChunk::BlockStart {
                index,
                block_type: ContentBlockType::from_tag(
                    value.as_str_at("blockType").unwrap_or("text"),
                ),
            }),
            "text-delta" => Ok(StreamChunk::TextDelta {
                index,
                text: value
                    .get("text")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            "reasoning-delta" => Ok(StreamChunk::ReasoningDelta {
                index,
                text: value
                    .get("text")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            "tool-call-delta" => Ok(StreamChunk::ToolCallDelta {
                index,
                id: CallId(
                    value
                        .get("id")
                        .and_then(JsonValue::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
                name: value
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .map(String::from),
                arguments_delta: value
                    .get("argumentsDelta")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            "block-end" => Ok(StreamChunk::BlockEnd {
                index,
                block: ContentBlock::from_json(
                    value.get("block").cloned().unwrap_or(JsonValue::Null),
                )?,
            }),
            "usage" => Ok(StreamChunk::Usage {
                usage: TokenUsage {
                    input_tokens: value
                        .get("inputTokens")
                        .and_then(JsonValue::as_u64)
                        .unwrap_or(0),
                    output_tokens: value
                        .get("outputTokens")
                        .and_then(JsonValue::as_u64)
                        .unwrap_or(0),
                    cache_read_tokens: value.get("cacheReadTokens").and_then(JsonValue::as_u64),
                    cache_write_tokens: value.get("cacheWriteTokens").and_then(JsonValue::as_u64),
                    reasoning_tokens: value.get("reasoningTokens").and_then(JsonValue::as_u64),
                },
            }),
            "finish" => Ok(StreamChunk::Finish {
                reason: FinishReason::from_json(
                    value.get("reason").cloned().unwrap_or(JsonValue::Null),
                )?,
                replay_state: value.get("replayState").map(|state| ReplayEnvelope {
                    response: state.get("response").cloned().unwrap_or(JsonValue::Null),
                    blocks: state
                        .get("blocks")
                        .and_then(JsonValue::as_array)
                        .map(|blocks| blocks.to_vec()),
                }),
            }),
            _ => Err(JsonValueError::NotObject),
        }
    }
}

impl LlmFailure {
    pub(crate) fn to_json(&self) -> JsonValue {
        let mut entries = vec![
            ("message".into(), JsonValue::String(self.message.clone())),
            ("code".into(), JsonValue::String(self.code.clone())),
        ];
        if let Some(status) = self.status {
            entries.push(("status".into(), JsonValue::UInt(status as u64)));
        }
        if let Some(delay) = self.provider_retry_after_ms {
            entries.push(("providerRetryAfterMs".into(), JsonValue::UInt(delay)));
        }
        if let Some(request_id) = &self.request_id {
            entries.push(("requestId".into(), JsonValue::String(request_id.clone())));
        }
        JsonValue::Object(entries)
    }

    fn from_json(value: &JsonValue) -> LlmFailure {
        LlmFailure {
            message: value
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string(),
            code: value
                .get("code")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string(),
            status: value
                .get("status")
                .and_then(JsonValue::as_u64)
                .filter(|status| *status <= u16::MAX as u64)
                .map(|status| status as u16),
            provider_retry_after_ms: value
                .get("providerRetryAfterMs")
                .and_then(JsonValue::as_u64),
            request_id: value
                .get("requestId")
                .and_then(JsonValue::as_str)
                .map(String::from),
        }
    }
}

impl FinishReason {
    pub fn to_json(&self) -> Result<JsonValue, JsonValueError> {
        let mut entries = Vec::new();
        let tag = match self {
            FinishReason::Stop => "stop",
            FinishReason::ToolCalls => "tool-calls",
            FinishReason::MaxTokens => "max-tokens",
            FinishReason::Aborted { failure } => {
                entries.push(("failure".into(), failure.to_json()));
                "aborted"
            }
            FinishReason::Error { failure } => {
                entries.push(("failure".into(), failure.to_json()));
                "error"
            }
            FinishReason::Custom { kind, failure } => {
                // Reserved kinds would parse back as the BUILT-IN variant
                // (FinishReason::Custom{kind:"stop"} round-tripped to Stop).
                if matches!(
                    kind.as_str(),
                    "stop" | "tool-calls" | "max-tokens" | "aborted" | "error"
                ) {
                    return Err(JsonValueError::DuplicateTagKey(kind.clone()));
                }
                let object = match failure {
                    Some(failure) => JsonValue::Object(vec![("failure".into(), failure.to_json())]),
                    None => JsonValue::Object(Vec::new()),
                };
                return JsonValue::with_tag("kind", kind, object);
            }
        };
        entries.insert(0, ("kind".into(), JsonValue::String(tag.into())));
        Ok(JsonValue::Object(entries))
    }

    pub fn from_json(value: JsonValue) -> Result<FinishReason, JsonValueError> {
        let (tag, fields) = value.split_tag("kind").ok_or(JsonValueError::NotObject)?;
        match tag.as_str() {
            "stop" => Ok(FinishReason::Stop),
            "tool-calls" => Ok(FinishReason::ToolCalls),
            "max-tokens" => Ok(FinishReason::MaxTokens),
            "aborted" => Ok(FinishReason::Aborted {
                failure: LlmFailure::from_json(fields.get("failure").unwrap_or(&JsonValue::Null)),
            }),
            "error" => Ok(FinishReason::Error {
                failure: LlmFailure::from_json(fields.get("failure").unwrap_or(&JsonValue::Null)),
            }),
            other => Ok(FinishReason::Custom {
                kind: other.to_string(),
                failure: fields.get("failure").map(LlmFailure::from_json),
            }),
        }
    }
}

#[cfg(test)]
mod tests {

    /// WI-47 Phase B dual-format replay: `arguments` on the wire may be the
    /// new lossless object OR a legacy string (old session logs). Both read
    /// back identically; a malformed legacy string keeps its raw text.
    #[test]
    fn tool_call_wire_accepts_string_and_object_arguments() {
        let object = JsonValue::Object(vec![
            ("cmd".into(), JsonValue::String("ls".into())),
            ("big".into(), JsonValue::UInt(18_446_744_073_709_551_615)),
        ]);
        let base = JsonValue::Object(vec![
            ("type".into(), JsonValue::String("tool-call".into())),
            ("id".into(), JsonValue::String("c1".into())),
            ("name".into(), JsonValue::String("bash".into())),
        ]);

        // New form: object arguments ride through untouched.
        let mut wire = base.clone();
        if let JsonValue::Object(entries) = &mut wire {
            entries.push(("arguments".into(), object.clone()));
        }
        let block = ContentBlock::from_json(wire).unwrap();
        let ContentBlock::ToolCall { arguments, .. } = block else {
            panic!("expected tool-call");
        };
        assert_eq!(arguments, object);

        // Legacy form: a string argument is parsed once through the shared
 // ingress rule (semantics: empty → {}).
        let mut legacy = base.clone();
        if let JsonValue::Object(entries) = &mut legacy {
            entries.push((
                "arguments".into(),
                JsonValue::String(r#"{"cmd":"ls","big":18446744073709551615}"#.into()),
            ));
        }
        let block = ContentBlock::from_json(legacy).unwrap();
        let ContentBlock::ToolCall { arguments, .. } = block else {
            panic!("expected tool-call");
        };
        assert_eq!(arguments, object);

        // Legacy malformed text keeps its raw form (no silent Null).
        let mut bad = base;
        if let JsonValue::Object(entries) = &mut bad {
            entries.push(("arguments".into(), JsonValue::String("{broken".into())));
        }
        let block = ContentBlock::from_json(bad).unwrap();
        let ContentBlock::ToolCall { arguments, .. } = block else {
            panic!("expected tool-call");
        };
        assert_eq!(arguments, JsonValue::String("{broken".into()));
    }
    use super::*;

    #[test]
    fn content_block_json_round_trip() {
        let cases = vec![
            ContentBlock::Text("hi".into()),
            ContentBlock::Reasoning("think".into()),
            ContentBlock::Image {
                attachment: JsonValue::Object(vec![("ref".into(), JsonValue::String("a1".into()))]),
            },
            ContentBlock::ToolCall {
                id: CallId("c1".into()),
                name: Some("bash".into()),
                arguments: JsonValue::Object(vec![("cmd".into(), JsonValue::String("ls".into()))]),
            },
            ContentBlock::ToolResult {
                call_id: CallId("c1".into()),
                content: vec![ContentBlock::Text("ok".into())],
                is_error: false,
            },
            ContentBlock::Custom {
                kind: "video".into(),
                payload: JsonValue::Object(vec![(
                    "url".into(),
                    JsonValue::String("https://x".into()),
                )]),
            },
        ];
        for block in cases {
            let json = block.to_json().unwrap();
            let back = ContentBlock::from_json(json).unwrap();
            assert_eq!(format!("{back:?}"), format!("{block:?}"));
        }
    }

    #[test]
    fn custom_block_wire_shape_has_type_tag() {
        let block = ContentBlock::Custom {
            kind: "video".into(),
            payload: JsonValue::Object(vec![("url".into(), JsonValue::String("https://x".into()))]),
        };
        let json = block.to_json().unwrap();
        assert_eq!(json.as_str_at("type"), Some("video"));
        assert_eq!(
            json.get("url").and_then(JsonValue::as_str),
            Some("https://x")
        );
    }

    #[test]
    fn custom_block_from_json_rejects_non_object_payload() {
        let error = ContentBlock::from_json(JsonValue::String("raw".into())).unwrap_err();
        assert!(matches!(error, JsonValueError::NotObject));
    }

    #[test]
    fn message_source_json_round_trip() {
        let cases = vec![
            MessageSource::User,
            MessageSource::Plugin {
                plugin: "p".into(),
                form: Some(ContextForm::Notice {
                    summary: "hello".into(),
                }),
            },
            MessageSource::Model {
                provider: "fake".into(),
                model: "m".into(),
                replay_state: Some(JsonValue::Object(vec![("k".into(), JsonValue::Int(1))])),
            },
            MessageSource::Tool {
                call_id: CallId("c1".into()),
            },
            MessageSource::Custom {
                kind: "sensor".into(),
                payload: JsonValue::Object(vec![("v".into(), JsonValue::Int(7))]),
            },
        ];
        for source in cases {
            let json = source.to_json().unwrap();
            let back = MessageSource::from_json(json).unwrap();
            assert_eq!(format!("{back:?}"), format!("{source:?}"));
        }
    }

    #[test]
    fn finish_reason_json_round_trip() {
        let failure = LlmFailure {
            message: "boom".into(),
            code: "SERVER".into(),
            status: Some(500),
            provider_retry_after_ms: None,
            request_id: Some("r1".into()),
        };
        let cases = vec![
            FinishReason::Stop,
            FinishReason::ToolCalls,
            FinishReason::MaxTokens,
            FinishReason::Error {
                failure: failure.clone(),
            },
            FinishReason::Aborted {
                failure: failure.clone(),
            },
            FinishReason::Custom {
                kind: "video-failed".into(),
                failure: Some(failure),
            },
        ];
        for reason in cases {
            let json = reason.to_json().unwrap();
            let back = FinishReason::from_json(json).unwrap();
            assert_eq!(format!("{back:?}"), format!("{reason:?}"));
        }
    }

    #[test]
    fn context_form_json_round_trip() {
        let form = ContextForm::Snapshot {
            sections: vec![ContextSnapshotSection {
                name: "fs".into(),
                text: "rules".into(),
            }],
            truncated: false,
            budget_bytes: 0,
            composed_bytes: 0,
        };
        let json = form.to_json().unwrap();
        let back = ContextForm::from_json(json).unwrap();
        assert_eq!(format!("{back:?}"), format!("{form:?}"));
    }

    /// WI-35: truncation metadata survives the wire round trip, and
    /// pre-WI-35 payloads (absent keys) read back as default false/0.
    #[test]
    fn context_form_snapshot_truncation_metadata_round_trips() {
        let form = ContextForm::Snapshot {
            sections: vec![],
            truncated: true,
            budget_bytes: 65_536,
            composed_bytes: 90_000,
        };
        let json = form.to_json().unwrap();
        assert_eq!(
            json.get("truncated").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            json.get("budgetBytes").and_then(JsonValue::as_u64),
            Some(65_536)
        );
        let back = ContextForm::from_json(json).unwrap();
        assert_eq!(back, form);

        // Pre-WI-35 payload: no truncation keys.
        let legacy = JsonValue::Object(vec![("form".into(), JsonValue::String("snapshot".into()))]);
        let back = ContextForm::from_json(legacy).unwrap();
        assert_eq!(
            back,
            ContextForm::Snapshot {
                sections: vec![],
                truncated: false,
                budget_bytes: 0,
                composed_bytes: 0,
            }
        );
    }

    #[test]
    fn message_json_round_trip() {
        let cases = vec![
            Message {
                id: MessageId("m1".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::Text("hello".into())],
                source: MessageSource::User,
            },
            Message {
                id: MessageId("m2".into()),
                role: MessageRole::Assistant,
                content: vec![
                    ContentBlock::Reasoning("think".into()),
                    ContentBlock::ToolCall {
                        id: CallId("c1".into()),
                        name: Some("bash".into()),
                        arguments: JsonValue::Object(Vec::new()),
                    },
                ],
                source: MessageSource::Model {
                    provider: "fake".into(),
                    model: "m".into(),
                    replay_state: None,
                },
            },
            Message {
                id: MessageId("m3".into()),
                role: MessageRole::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: CallId("c1".into()),
                    content: vec![ContentBlock::Text("ok".into())],
                    is_error: false,
                }],
                source: MessageSource::Tool {
                    call_id: CallId("c1".into()),
                },
            },
        ];
        for message in cases {
            let json = message.to_json().unwrap();
            let back = Message::from_json(json).unwrap();
            assert_eq!(format!("{back:?}"), format!("{message:?}"));
        }
    }

    #[test]
    fn message_from_json_requires_role_and_source() {
        let error = Message::from_json(JsonValue::Object(vec![(
            "id".into(),
            JsonValue::String("m1".into()),
        )]))
        .unwrap_err();
        assert!(matches!(error, JsonValueError::NotObject));
    }

    #[test]
    fn stream_chunk_json_round_trip() {
        let failure = LlmFailure {
            message: "boom".into(),
            code: "SERVER".into(),
            status: Some(500),
            provider_retry_after_ms: None,
            request_id: None,
        };
        let cases = vec![
            StreamChunk::BlockStart {
                index: 0,
                block_type: ContentBlockType::Custom("video".into()),
            },
            StreamChunk::TextDelta {
                index: 0,
                text: "hi".into(),
            },
            StreamChunk::ReasoningDelta {
                index: 1,
                text: "think".into(),
            },
            StreamChunk::ToolCallDelta {
                index: 2,
                id: CallId("c1".into()),
                name: Some("bash".into()),
                arguments_delta: "{}".into(),
            },
            StreamChunk::BlockEnd {
                index: 0,
                block: ContentBlock::Text("hi".into()),
            },
            StreamChunk::Usage {
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 2,
                    cache_read_tokens: Some(3),
                    cache_write_tokens: None,
                    reasoning_tokens: Some(4),
                },
            },
            StreamChunk::Finish {
                reason: FinishReason::Error { failure },
                replay_state: Some(ReplayEnvelope {
                    response: JsonValue::Object(vec![("k".into(), JsonValue::Int(1))]),
                    blocks: Some(vec![JsonValue::String("b".into())]),
                }),
            },
        ];
        for chunk in cases {
            let json = chunk.to_json().unwrap();
            let back = StreamChunk::from_json(json).unwrap();
            assert_eq!(format!("{back:?}"), format!("{chunk:?}"));
        }
    }
}

// ── Round-6 regressions: Custom wire collisions ──
#[cfg(test)]
mod round6_tests {
    use super::*;

    #[test]
    fn custom_reserved_kind_is_rejected_at_emission() {
        let block = ContentBlock::Custom {
            kind: "text".into(),
            payload: crate::json::JsonValue::Object(vec![]),
        };
        assert!(
            block.to_json().is_err(),
            "a reserved-kind Custom must fail loudly, not silently become Text"
        );
    }

    #[test]
    fn custom_payload_carrying_the_tag_key_is_rejected() {
        let block = ContentBlock::Custom {
            kind: "video".into(),
            payload: crate::json::JsonValue::Object(vec![(
                "type".into(),
                crate::json::JsonValue::String("mp4".into()),
            )]),
        };
        assert!(
            block.to_json().is_err(),
            "a payload with its own `type` key would round-trip minus that field"
        );
        // Legal payloads keep working.
        let ok = ContentBlock::Custom {
            kind: "video".into(),
            payload: crate::json::JsonValue::Object(vec![(
                "url".into(),
                crate::json::JsonValue::String("https://x".into()),
            )]),
        };
        let back = ContentBlock::from_json(ok.to_json().unwrap()).unwrap();
        assert!(matches!(back, ContentBlock::Custom { kind, .. } if kind == "video"));
    }
}
