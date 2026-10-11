use serde::{Deserialize, Serialize};

/// Unified message format — OpenAI Chat Completions compatible
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    /// Compaction bookkeeping. `None` for ordinary messages; set by the LLM
    /// summarization pass (phase 2) to track compression state across turns.
    #[serde(default)]
    pub meta: Option<MessageMeta>,
}

/// Compaction bookkeeping attached to a message (§5.3.4 MessageMeta).
///
/// Never sent to the provider — the wire-format converters only read `role`/`content`,
/// so this is server-side metadata ignored by `to_openai_messages` / `convert_messages`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct MessageMeta {
    /// Unique per-message id, assigned once and never changed.
    #[serde(default)]
    pub msg_id: Option<String>,
    /// true → this message IS a compression summary (System role, structured JSON body).
    #[serde(default)]
    pub is_compact_summary: bool,
    /// true → this message's content was consumed by a summary; excluded from
    /// subsequent L1/L3 block selection (anti-double-compress, §5.3.5).
    #[serde(default)]
    pub is_compressed: bool,
    /// Original msg_ids that this summary replaces (for traceability).
    #[serde(default)]
    pub source_msg_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    /// Chain-of-thought / reasoning content (DeepSeek thinking mode).
    /// MUST be passed back to the API in assistant messages when the turn made
    /// tool calls, or DeepSeek returns 400 "reasoning_content must be passed back".
    #[serde(rename = "thinking")]
    Thinking { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

impl Message {
    pub fn system(content: &str) -> Self {
        Self {
            role: Role::System,
            content: vec![ContentBlock::Text {
                text: content.to_string(),
            }],
            meta: None,
        }
    }
    pub fn user(content: &str) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: content.to_string(),
            }],
            meta: None,
        }
    }
    pub fn assistant(content: &str) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: content.to_string(),
            }],
            meta: None,
        }
    }
    pub fn tool_result(tool_use_id: &str, result: &str, is_error: bool) -> Self {
        Self {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: result.to_string(),
                is_error,
            }],
            meta: None,
        }
    }
    /// Builder: attach compaction metadata. `None`-normal messages can chain
    /// `.with_meta(...)` without touching the plaintext constructors.
    pub fn with_meta(mut self, meta: MessageMeta) -> Self {
        self.meta = Some(meta);
        self
    }
}

/// Tool definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Agent event — SSE stream pushed to callers
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentEvent {
    TaskStarted {
        session_id: String,
    },
    TaskCompleted {
        session_id: String,
        summary: String,
    },
    TaskFailed {
        session_id: String,
        error: String,
    },
    TaskCancelled {
        session_id: String,
    },
    ThinkingStart,
    ThinkingDelta {
        text: String,
    },
    ThinkingDone,
    TextDelta {
        text: String,
    },
    TextDone,
    ToolCallStart {
        id: String,
        name: String,
        params: serde_json::Value,
    },
    ToolCallInput {
        id: String,
        delta: String,
    },
    ToolCallDone {
        id: String,
        name: String,
        index: u32,
        input: serde_json::Value,
    },
    ToolCallResult {
        id: String,
        ok: bool,
        output: String,
    },
    ToolCallError {
        id: String,
        error: String,
    },
    /// Compaction applied. `level` is the executed action label: a `CompactionLevel`
    /// variant ("Level0".."Critical"), or "ForceTruncate" / "PreSendGuard" for the
    /// hard fallback paths that run outside the tiered watermark decision.
    CompactionTriggered {
        level: String,
        before_tokens: usize,
        after_tokens: usize,
    },
    ApprovalRequired {
        tool: String,
        params: serde_json::Value,
        reason: String,
    },
    /// Bridge tool — requires JS-side execution (MCP tools)
    BridgeToolExecute {
        session_id: String,
        call_id: String,
        tool_name: String,
        input: serde_json::Value,
    },
    /// Bridge tool complete — JS-side callback
    BridgeToolComplete {
        session_id: String,
        call_id: String,
        output: String,
        success: bool,
    },
    /// Debug: exact Chat Completion API request body for a turn. Not persisted.
    RequestLog {
        session_id: String,
        turn: usize,
        body: serde_json::Value,
    },
    /// Debug: raw SSE response chunks for a turn, batched. Not persisted.
    ResponseLog {
        session_id: String,
        turn: usize,
        chunks: Vec<String>,
    },
}

/// Error detail — preserve type info for differentiated TUI rendering
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub kind: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Session info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub status: String,
    pub model: String,
    pub turns: usize,
    pub tokens_used: usize,
    pub created_at: i64,
}

/// Model info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub context_window: usize,
    pub max_tokens: usize,
}

/// Generation parameters configurable per model definition
#[derive(Debug, Clone)]
pub struct GenerationConfig {
    pub max_tokens: u32,
    pub temperature: f32,
    pub context_window: usize,
    /// Wire protocol format: "openai-chat" (default) | "openai-responses" |
    /// "anthropic"
    pub api_format: String,
    /// DeepSeek thinking mode control. None = don't send (provider default).
    /// "off" → thinking disabled; "low"/"high"/"max" → reasoning_effort level.
    /// Note: temperature/top_p are IGNORED by DeepSeek while thinking mode is enabled.
    pub reasoning_effort: Option<String>,
}

impl Default for GenerationConfig {
    fn default() -> Self {
        Self {
            max_tokens: 8192,
            temperature: 0.7,
            context_window: 128000,
            api_format: "openai-chat".to_string(),
            reasoning_effort: None,
        }
    }
}

/// Inference engine health status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    pub engine_id: String,
    pub status: HealthState,
    pub latency_ms: u64,
    pub available_models: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthState {
    Healthy,
    Degraded,
    Unhealthy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_serde_tags_are_pascal_case() {
        // Frontend dispatches on event.type matching the PascalCase variant name.
        // Debug events must follow the same convention as TaskStarted/TextDelta/etc.
        let req = AgentEvent::RequestLog {
            session_id: "s".into(),
            turn: 0,
            body: serde_json::json!({"model": "m"}),
        };
        let j: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            j["type"], "RequestLog",
            "RequestLog must serialize as PascalCase"
        );
        let resp = AgentEvent::ResponseLog {
            session_id: "s".into(),
            turn: 0,
            chunks: vec!["{}".into()],
        };
        let j: serde_json::Value = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            j["type"], "ResponseLog",
            "ResponseLog must serialize as PascalCase"
        );
        // Existing convention sanity check
        let started = AgentEvent::TaskStarted {
            session_id: "s".into(),
        };
        let j: serde_json::Value = serde_json::to_value(&started).unwrap();
        assert_eq!(j["type"], "TaskStarted");
    }

    #[test]
    fn generation_config_defaults() {
        let cfg = GenerationConfig::default();
        assert_eq!(cfg.max_tokens, 8192);
        assert_eq!(cfg.temperature, 0.7);
        assert_eq!(cfg.context_window, 128000);
        assert_eq!(cfg.api_format, "openai-chat");
    }

    #[test]
    fn constructors_build_correct_single_blocks() {
        let m = Message::system("sys");
        assert!(matches!(m.role, Role::System));
        assert_eq!(m.content.len(), 1);
        assert!(matches!(m.content[0], ContentBlock::Text { ref text } if text == "sys"));

        let m = Message::user("u");
        assert!(matches!(m.role, Role::User));
        assert!(matches!(m.content[0], ContentBlock::Text { ref text } if text == "u"));

        let m = Message::assistant("a");
        assert!(matches!(m.role, Role::Assistant));
        assert!(matches!(m.content[0], ContentBlock::Text { ref text } if text == "a"));

        let m = Message::tool_result("tid", "res", true);
        assert!(matches!(m.role, Role::Tool));
        assert!(
            matches!(m.content[0], ContentBlock::ToolResult { ref tool_use_id, ref is_error, ref content }
            if tool_use_id.as_str() == "tid" && content.as_str() == "res" && *is_error)
        );
    }

    #[test]
    fn role_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            "\"assistant\""
        );
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"tool\"");
        assert_eq!(
            serde_json::from_str::<Role>("\"user\"").unwrap(),
            Role::User
        );
    }

    #[test]
    fn content_block_type_tags() {
        let blocks = vec![
            ContentBlock::Text { text: "t".into() },
            ContentBlock::ToolUse {
                id: "i".into(),
                name: "n".into(),
                input: serde_json::json!({}),
            },
            ContentBlock::ToolResult {
                tool_use_id: "i".into(),
                content: "c".into(),
                is_error: false,
            },
        ];
        let v = serde_json::to_value(&blocks).unwrap();
        assert_eq!(v[0]["type"], "text");
        assert_eq!(v[1]["type"], "tool_use");
        assert_eq!(v[2]["type"], "tool_result");
        assert_eq!(v[2]["is_error"], false);
    }

    #[test]
    fn tool_result_is_error_defaults_false_when_absent() {
        let json = r#"{"type":"tool_result","tool_use_id":"x","content":"y"}"#;
        let cb: ContentBlock = serde_json::from_str(json).unwrap();
        assert!(matches!(
            cb,
            ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ));
    }

    #[test]
    fn message_round_trip() {
        let m = Message::user("hello");
        let s = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&s).unwrap();
        assert!(matches!(back.role, Role::User));
        assert!(matches!(back.content[0], ContentBlock::Text { ref text } if text == "hello"));
    }
}
