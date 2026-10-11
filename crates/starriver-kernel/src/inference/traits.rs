use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::types::{HealthStatus, Message, ModelInfo, ToolDefinition};

/// Stream events returned by inference engine (internal unified format, OpenAI compatible)
#[derive(Debug, Clone)]
pub enum StreamEvent {
    ThinkingStart,
    ThinkingDelta(String),
    ThinkingDone,
    TextDelta(String),
    TextDone,
    /// Stream ended with a non-normal finish_reason: "length" (truncated by max_tokens),
    /// "content_filter", "insufficient_system_resource" (DeepSeek-specific), or others.
    FinishReason(String),
    /// Debug: exact request body sent to the API (emitted as the first stream event).
    RequestLog(serde_json::Value),
    /// Debug: a raw SSE `data:` line received from the API (before parsing), so
    /// malformed/truncated chunks are still visible.
    ResponseChunk(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallInput {
        id: String,
        delta: String,
    },
    ToolCallDone {
        id: String,
        input: serde_json::Value,
    },
    Error(String),
}

/// Unified inference engine interface
#[async_trait]
pub trait InferenceEngine: Send + Sync {
    /// Engine identifier
    fn id(&self) -> &str;

    /// Engine type name
    fn name(&self) -> &str;

    /// Streaming chat — core method
    async fn stream_chat(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<BoxStream<'static, Result<StreamEvent, String>>, String>;

    /// Non-streaming chat — used for context compaction
    async fn chat(&self, model: &str, messages: &[Message]) -> Result<String, String>;

    /// Health check
    async fn health(&self) -> HealthStatus;

    /// List available models
    fn models(&self) -> Vec<ModelInfo>;

    /// Max recommended concurrency
    fn max_concurrency(&self) -> usize;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_event_all_variants_construct_and_debug() {
        // Every variant must be constructible and Debug-printable without panicking.
        let events = vec![
            StreamEvent::ThinkingStart,
            StreamEvent::ThinkingDelta("thinking…".into()),
            StreamEvent::ThinkingDone,
            StreamEvent::TextDelta("text".into()),
            StreamEvent::TextDone,
            StreamEvent::FinishReason("length".into()),
            StreamEvent::RequestLog(serde_json::json!({"model": "m"})),
            StreamEvent::ResponseChunk("data: {...}".into()),
            StreamEvent::ToolCallStart {
                id: "t1".into(),
                name: "read".into(),
            },
            StreamEvent::ToolCallInput {
                id: "t1".into(),
                delta: "{\"path\"".into(),
            },
            StreamEvent::ToolCallDone {
                id: "t1".into(),
                input: serde_json::json!({"path": "x"}),
            },
            StreamEvent::Error("boom".into()),
        ];
        for e in &events {
            let _ = format!("{e:?}");
        }
        assert_eq!(events.len(), 12);
    }

    #[test]
    fn stream_event_multibyte_delta_survives() {
        let ev = StreamEvent::TextDelta("第一段中文".into());
        assert!(matches!(ev, StreamEvent::TextDelta(t) if t == "第一段中文"));
    }

    #[test]
    fn tool_call_done_carries_parsed_json_input() {
        let ev = StreamEvent::ToolCallDone {
            id: "t".into(),
            input: serde_json::json!({"command": "ls -la"}),
        };
        match ev {
            StreamEvent::ToolCallDone { id, input } => {
                assert_eq!(id, "t");
                assert_eq!(input["command"], "ls -la");
            }
            _ => panic!("wrong variant"),
        }
    }
}
