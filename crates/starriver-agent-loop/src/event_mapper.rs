//! Legacy-event bridge (A1 retirement): maps kernel `SessionEvent`s onto
//! the core `AgentEvent` vocabulary that the mobile (C ABI) and edge (SSE)
//! product surfaces stream to their clients. One mapper per execute() run.

use starriver_kernel::json::JsonValue;
use starriver_kernel::session::SessionEvent;
use starriver_kernel::types::AgentEvent;

/// Stateful mapper: tracks the last assistant text so the completed-task
/// summary carries the model's final answer.
pub struct EventMapper {
    session_id: String,
    last_assistant: String,
}

impl EventMapper {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            last_assistant: String::new(),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn feed(&mut self, event: &SessionEvent) -> Vec<AgentEvent> {
        match event.event_type.as_str() {
            "turn/start" => {
                vec![AgentEvent::TaskStarted {
                    session_id: self.session_id.clone(),
                }]
            }
            "assistant/chunk" => self.feed_chunk(event),
            "tool/call" => {
                let id = event
                    .data
                    .as_str_at("callId")
                    .unwrap_or_default()
                    .to_string();
                let name = event.data.as_str_at("name").unwrap_or_default().to_string();
                // WI-47 Phase B dual-format: old logs store `arguments` as
                // a string, new logs as the lossless object.
                let params: serde_json::Value = match event.data.get("arguments") {
                    Some(JsonValue::String(text)) => {
                        serde_json::from_str(text).unwrap_or(serde_json::Value::Null)
                    }
                    Some(other) => other.to_serde_json().unwrap_or(serde_json::Value::Null),
                    None => serde_json::Value::Null,
                };
                vec![AgentEvent::ToolCallStart { id, name, params }]
            }
            "tool/result" => {
                let id = event
                    .data
                    .get("message")
                    .and_then(|m| m.get("source"))
                    .and_then(|s| s.as_str_at("callId"))
                    .unwrap_or_default()
                    .to_string();
                let ok = event.data.get("error").is_none();
                let output = event
                    .data
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(JsonValue::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter_map(|b| b.get("text").and_then(JsonValue::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                vec![AgentEvent::ToolCallResult { id, ok, output }]
            }
            "assistant/message" => {
                if let Some(message_value) = event.data.get("message") {
                    if let Some(blocks) = message_value.get("content").and_then(JsonValue::as_array)
                    {
                        for block in blocks {
                            if block.get("type").and_then(JsonValue::as_str) == Some("text") {
                                if let Some(text) = block.get("text").and_then(JsonValue::as_str) {
                                    self.last_assistant = text.to_string();
                                }
                            }
                        }
                    }
                }
                Vec::new()
            }
            "turn/end" => {
                let kind = event
                    .data
                    .get("reason")
                    .and_then(|r| r.as_str_at("kind"))
                    .unwrap_or("completed")
                    .to_string();
                match kind.as_str() {
                    "completed" => vec![AgentEvent::TaskCompleted {
                        session_id: self.session_id.clone(),
                        summary: std::mem::take(&mut self.last_assistant),
                    }],
                    "aborted" | "disposed" => {
                        vec![AgentEvent::TaskCancelled {
                            session_id: self.session_id.clone(),
                        }]
                    }
                    _ => {
                        // max-tokens/blocked turns carry no failure object —
                        // surface the actual kind instead of a fabricated
                        // "unknown error" TaskFailed.
                        let error = event
                            .data
                            .get("reason")
                            .and_then(|r| r.get("failure"))
                            .and_then(|f| f.as_str_at("message"))
                            .map(String::from)
                            .unwrap_or_else(|| format!("turn ended: {kind}"));
                        vec![AgentEvent::TaskFailed {
                            session_id: self.session_id.clone(),
                            error,
                        }]
                    }
                }
            }
            _ => Vec::new(),
        }
    }

    fn feed_chunk(&mut self, event: &SessionEvent) -> Vec<AgentEvent> {
        let Some(chunk_value) = event.data.get("chunk").cloned() else {
            return Vec::new();
        };
        let tag = chunk_value
            .get("type")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_string();
        match tag.as_str() {
            "text-delta" => {
                let text = chunk_value
                    .get("text")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string();
                vec![AgentEvent::TextDelta { text }]
            }
            "reasoning-delta" => {
                let text = chunk_value
                    .get("text")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string();
                vec![AgentEvent::ThinkingDelta { text }]
            }
            "tool-call-delta" => {
                let id = chunk_value
                    .get("id")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string();
                let delta = chunk_value
                    .get("argumentsDelta")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string();
                vec![AgentEvent::ToolCallInput { id, delta }]
            }
            "block-start" => {
                // The tool-call name arrives on the first delta; text/reasoning
                // starts map to nothing (the deltas carry the payload).
                Vec::new()
            }
            "block-end" => {
                let block = chunk_value.get("block").cloned().unwrap_or(JsonValue::Null);
                let block_type = block
                    .get("type")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_string();
                match block_type.as_str() {
                    "text" => vec![AgentEvent::TextDone],
                    "reasoning" => vec![AgentEvent::ThinkingDone],
                    "tool-call" => {
                        let id = block
                            .get("id")
                            .and_then(JsonValue::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(JsonValue::as_str)
                            .unwrap_or_default()
                            .to_string();
                        // Dual-format (old string / new object), see
                        // tool/call above.
                        let input: serde_json::Value = match block.get("arguments") {
                            Some(JsonValue::String(text)) => {
                                serde_json::from_str(text).unwrap_or(serde_json::Value::Null)
                            }
                            Some(other) => other.to_serde_json().unwrap_or(serde_json::Value::Null),
                            None => serde_json::Value::Null,
                        };
                        vec![AgentEvent::ToolCallDone {
                            id,
                            name,
                            index: 0,
                            input,
                        }]
                    }
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_kernel(value: &serde_json::Value) -> JsonValue {
        use serde_json::Value as V;
        match value {
            V::Null => JsonValue::Null,
            V::Bool(b) => JsonValue::Bool(*b),
            V::Number(n) => JsonValue::String(n.to_string()),
            V::String(t) => JsonValue::String(t.clone()),
            V::Array(items) => JsonValue::Array(items.iter().map(to_kernel).collect()),
            V::Object(entries) => JsonValue::Object(
                entries
                    .iter()
                    .map(|(k, v)| (k.clone(), to_kernel(v)))
                    .collect(),
            ),
        }
    }

    fn chunk_event(chunk: serde_json::Value) -> SessionEvent {
        SessionEvent {
            seq: 0,
            time: 0,
            event_type: "assistant/chunk".into(),
            data: JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(1)),
                ("step".into(), JsonValue::UInt(1)),
                ("chunk".into(), to_kernel(&chunk)),
            ]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        }
    }

    #[test]
    fn stream_and_turn_events_map_onto_agent_events() {
        let mut mapper = EventMapper::new("s1".into());
        let mut out = mapper.feed(&chunk_event(
            serde_json::json!({"type": "text-delta", "index": 0, "text": "hi"}),
        ));
        out.extend(mapper.feed(&chunk_event(
            serde_json::json!({"type": "block-end", "index": 0,
                "block": {"type": "text", "text": "hi"}}),
        )));
        assert!(matches!(out[0], AgentEvent::TextDelta { ref text } if text == "hi"));
        assert!(matches!(out[1], AgentEvent::TextDone));

        // The finalized assistant message feeds the completion summary.
        let assistant_message = SessionEvent {
            seq: 8,
            time: 0,
            event_type: "assistant/message".into(),
            data: JsonValue::Object(vec![(
                "message".into(),
                JsonValue::Object(vec![(
                    "content".into(),
                    JsonValue::Array(vec![JsonValue::Object(vec![
                        ("type".into(), JsonValue::String("text".into())),
                        ("text".into(), JsonValue::String("hi".into())),
                    ])]),
                )]),
            )]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        };
        let _ = mapper.feed(&assistant_message);

        let turn_end = SessionEvent {
            seq: 9,
            time: 0,
            event_type: "turn/end".into(),
            data: JsonValue::Object(vec![(
                "reason".into(),
                JsonValue::Object(vec![("kind".into(), JsonValue::String("completed".into()))]),
            )]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        };
        let out = mapper.feed(&turn_end);
        match out.last() {
            Some(AgentEvent::TaskCompleted {
                session_id,
                summary,
            }) => {
                assert_eq!(session_id, "s1");
                assert_eq!(summary, "hi");
            }
            other => panic!("expected TaskCompleted, got {other:?}"),
        }
    }

    #[test]
    fn error_turns_map_to_task_failed() {
        let mut mapper = EventMapper::new("s1".into());
        let event = SessionEvent {
            seq: 1,
            time: 0,
            event_type: "turn/end".into(),
            data: JsonValue::Object(vec![(
                "reason".into(),
                JsonValue::Object(vec![
                    ("kind".into(), JsonValue::String("error".into())),
                    (
                        "failure".into(),
                        JsonValue::Object(vec![(
                            "message".into(),
                            JsonValue::String("boom".into()),
                        )]),
                    ),
                ]),
            )]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        };
        let out = mapper.feed(&event);
        match out.last() {
            Some(AgentEvent::TaskFailed { error, .. }) => assert_eq!(error, "boom"),
            other => panic!("expected TaskFailed, got {other:?}"),
        }
    }
}

// WI-47 Phase C lossless regression (§5-3): `tool/call` → ToolCallStart
// params keep exact values in both the new (object) and legacy (string)
// event forms.
#[test]
fn tool_call_params_are_lossless_in_both_event_formats() {
    let raw = r#"{"big":18446744073709551615,"path":"/tmp/x"}"#;
    let object = JsonValue::from_json_str(raw).unwrap();
    let mk = |arguments: JsonValue| SessionEvent {
        seq: 1,
        time: 0,
        event_type: "tool/call".into(),
        data: JsonValue::Object(vec![
            ("callId".into(), JsonValue::String("c1".into())),
            ("name".into(), JsonValue::String("read".into())),
            ("arguments".into(), arguments),
        ]),
        surface_op: None,
        source_event_seqs: Vec::new(),
    };
    let expected = serde_json::json!({
        "big": 18_446_744_073_709_551_615u64,
        "path": "/tmp/x",
    });

    let mut mapper = EventMapper::new("s1".into());
    let out = mapper.feed(&mk(object));
    match out.as_slice() {
        [AgentEvent::ToolCallStart { params, .. }] => assert_eq!(*params, expected),
        other => panic!("expected ToolCallStart, got {other:?}"),
    }

    let mut mapper = EventMapper::new("s1".into());
    let out = mapper.feed(&mk(JsonValue::String(raw.into())));
    match out.as_slice() {
        [AgentEvent::ToolCallStart { params, .. }] => assert_eq!(*params, expected),
        other => panic!("expected ToolCallStart (legacy string), got {other:?}"),
    }
}
