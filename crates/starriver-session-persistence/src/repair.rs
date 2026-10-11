//! Cold crash repair: synthesize closers that balance an interrupted turn.

use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::{
    mint_message_id, CallId, ContentBlock, Message, MessageRole, MessageSource,
};
use starriver_kernel::session::SessionEvent;

pub const TOOL_NOT_STARTED: &str = "TOOL_NOT_STARTED";
pub const TOOL_OUTCOME_UNKNOWN: &str = "TOOL_OUTCOME_UNKNOWN";

/// A pending tool call discovered in the open tail turn.
#[derive(Debug, Clone)]
struct PendingCall {
    step: u64,
    call_seq: Option<u64>,
}

/// Tail-turn scan result: the open step (if any) plus every unresolved call.
type TailScan = (Option<u64>, Vec<(String, PendingCall)>);

/// Return synthetic `tool/result` / `step/end` / `turn/end{interrupted}`
/// closers for an open final turn, matching `interruptedTurnClosers`:
/// an assistant tool-call block without a recorded `tool/call` gets
/// `TOOL_NOT_STARTED`; a recorded call without a result gets
/// `TOOL_OUTCOME_UNKNOWN`.
pub fn closers_for(events: &[SessionEvent]) -> Vec<SessionEvent> {
    let Some(open_turn) = last_turn_start(events) else {
        return Vec::new();
    };
    let Some((open_step, pending)) = scan_tail(events, open_turn) else {
        // Balanced final turn.
        return Vec::new();
    };
    let mut seq = events.len() as u64;
    let time = events.last().map(|e| e.time).unwrap_or(0);
    let mut closers = Vec::new();
    // Close calls before their step: providers reject dangling assistant
    // calls, and insertion order preserves transcript order.
    for (call_id, pending) in pending {
        let started = pending.call_seq.is_some();
        let code = if started {
            TOOL_OUTCOME_UNKNOWN
        } else {
            TOOL_NOT_STARTED
        };
        let message = Message {
            id: mint_message_id(),
            role: MessageRole::User,
            source: MessageSource::Tool {
                call_id: CallId(call_id.clone()),
            },
            content: vec![ContentBlock::ToolResult {
                call_id: CallId(call_id.clone()),
                content: vec![ContentBlock::Text(
                    if started {
                        "The tool call was interrupted after it was recorded, but no result was durably recorded. Its outcome is unknown."
                    } else {
                        "The tool call was interrupted before it was recorded as started. Retry it if it is still needed."
                    }
                    .into(),
                )],
                is_error: true,
            }],
        };
        let mut data = vec![
            ("turn".into(), JsonValue::UInt(open_turn as u64)),
            ("step".into(), JsonValue::UInt(pending.step)),
            ("message".into(), message.to_json().expect("message json")),
            (
                "error".into(),
                JsonValue::Object(vec![
                    (
                        "name".into(),
                        JsonValue::String(
                            (if started {
                                "ToolOutcomeUnknownError"
                            } else {
                                "ToolNotStartedError"
                            })
                            .into(),
                        ),
                    ),
                    ("code".into(), JsonValue::String(code.into())),
                ]),
            ),
        ];
        if let Some(call_seq) = pending.call_seq {
            data.push(("sourceEventSeqs".into(), JsonValue::UInt(call_seq)));
        }
        closers.push(SessionEvent {
            seq,
            time,
            event_type: "tool/result".into(),
            data: JsonValue::Object(data),
            // Synthesized closers exist FOR the model surface — without an
            // Append op the fold skips them and the dangling tool call
            // stays dangling, defeating the repair's whole purpose.
            surface_op: Some(starriver_kernel::session::SurfaceOp::Append),
            source_event_seqs: pending.call_seq.into_iter().collect(),
        });
        seq += 1;
    }
    // Close an open step before the turn — a turn/end under an open step is
    // an invariant violation.
    if let Some(open_step) = open_step {
        closers.push(SessionEvent {
            seq,
            time,
            event_type: "step/end".into(),
            data: JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(open_turn as u64)),
                ("step".into(), JsonValue::UInt(open_step)),
            ]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        });
        seq += 1;
    }
    closers.push(SessionEvent {
        seq,
        time,
        event_type: "turn/end".into(),
        data: JsonValue::Object(vec![(
            "reason".into(),
            JsonValue::Object(vec![(
                "kind".into(),
                JsonValue::String("interrupted".into()),
            )]),
        )]),
        surface_op: None,
        source_event_seqs: Vec::new(),
    });
    closers
}

fn last_turn_start(events: &[SessionEvent]) -> Option<usize> {
    events.iter().rposition(|e| e.event_type == "turn/start")
}

/// Scan the tail turn after `open_turn`, tracking the open step and every
/// unresolved tool call. A resolved call (a `tool/result` matching its id)
/// is removed; `tool/call` events mark a call as started.
fn scan_tail(events: &[SessionEvent], open_turn: usize) -> Option<TailScan> {
    let mut open_step: Option<u64> = None;
    let mut pending: Vec<(String, PendingCall)> = Vec::new();
    for event in &events[open_turn..] {
        match event.event_type.as_str() {
            "step/start" => {
                open_step = event.data.get("step").and_then(JsonValue::as_u64);
            }
            "step/end" => {
                open_step = None;
                pending.clear();
            }
            "turn/end" => {
                return None;
            }
            "assistant/message" => {
                let step = event
                    .data
                    .get("step")
                    .and_then(JsonValue::as_u64)
                    .unwrap_or(0);
                if let Some(message) = event.data.get("message") {
                    if let Some(content) = message.get("content").and_then(JsonValue::as_array) {
                        for block in content {
                            if block.as_str_at("type") == Some("tool-call") {
                                if let Some(id) = block.get("id").and_then(JsonValue::as_str) {
                                    if !pending.iter().any(|(cid, _)| cid == id) {
                                        pending.push((
                                            id.to_string(),
                                            PendingCall {
                                                step,
                                                call_seq: None,
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            "tool/call" => {
                if let Some(id) = event.data.get("callId").and_then(JsonValue::as_str) {
                    if let Some((_, entry)) = pending.iter_mut().find(|(cid, _)| cid == id) {
                        entry.call_seq = Some(event.seq);
                    }
                }
            }
            "tool/result" => {
                if let Some(message) = event.data.get("message") {
                    if let Some(call_id) = message
                        .get("source")
                        .and_then(|s| s.get("callId"))
                        .and_then(JsonValue::as_str)
                    {
                        pending.retain(|(cid, _)| cid != call_id);
                    }
                }
            }
            _ => {}
        }
    }
    Some((open_step, pending))
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::llm::{ContentBlock, Message, MessageId, MessageRole, MessageSource};

    pub(super) fn event(seq: u64, event_type: &str) -> SessionEvent {
        SessionEvent {
            seq,
            time: 1,
            event_type: event_type.into(),
            data: JsonValue::Null,
            surface_op: None,
            source_event_seqs: Vec::new(),
        }
    }

    pub(super) fn tool_call_event(seq: u64, turn: u64, step: u64, call_id: &str) -> SessionEvent {
        SessionEvent {
            seq,
            time: 1,
            event_type: "tool/call".into(),
            data: JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(turn)),
                ("step".into(), JsonValue::UInt(step)),
                ("callId".into(), JsonValue::String(call_id.into())),
                ("name".into(), JsonValue::String("echo".into())),
                ("arguments".into(), JsonValue::String("{}".into())),
            ]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        }
    }

    pub(super) fn assistant_with_tool_calls(
        seq: u64,
        turn: u64,
        step: u64,
        call_ids: &[&str],
    ) -> SessionEvent {
        let message = Message {
            id: MessageId(format!("m{seq}")),
            role: MessageRole::Assistant,
            content: call_ids
                .iter()
                .map(|id| ContentBlock::ToolCall {
                    id: starriver_kernel::llm::CallId(id.to_string()),
                    name: Some("echo".into()),
                    arguments: JsonValue::Object(Vec::new()),
                })
                .collect(),
            source: MessageSource::Model {
                provider: "mock".into(),
                model: "mock".into(),
                replay_state: None,
            },
        };
        SessionEvent {
            seq,
            time: 1,
            event_type: "assistant/message".into(),
            data: JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(turn)),
                ("step".into(), JsonValue::UInt(step)),
                ("message".into(), message.to_json().unwrap()),
            ]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        }
    }

    fn step_start(seq: u64, turn: u64, step: u64) -> SessionEvent {
        let mut e = event(seq, "step/start");
        e.data = JsonValue::Object(vec![
            ("turn".into(), JsonValue::UInt(turn)),
            ("step".into(), JsonValue::UInt(step)),
        ]);
        e
    }

    #[test]
    fn balanced_log_produces_no_closers() {
        let events = vec![event(0, "turn/start"), event(1, "turn/end")];
        assert!(closers_for(&events).is_empty());
    }

    #[test]
    fn empty_log_produces_no_closers() {
        assert!(closers_for(&[]).is_empty());
    }

    #[test]
    fn open_turn_without_tool_calls_synthesizes_interrupted_end() {
        let events = vec![event(0, "turn/start"), event(1, "user/message")];
        let closers = closers_for(&events);
        assert_eq!(closers.len(), 1);
        let turn_end = &closers[0];
        assert_eq!(turn_end.event_type, "turn/end");
        let reason = turn_end
            .data
            .get("reason")
            .and_then(|r| r.as_str_at("kind"));
        assert_eq!(reason, Some("interrupted"));
    }

    #[test]
    fn unstarted_tool_call_synthesizes_not_started() {
        let events = vec![
            event(0, "turn/start"),
            assistant_with_tool_calls(1, 0, 0, &["call-1"]),
        ];
        let closers = closers_for(&events);
        assert_eq!(closers.len(), 2);
        let result = &closers[0];
        assert_eq!(result.event_type, "tool/result");
        let error = result.data.get("error").unwrap();
        assert_eq!(error.as_str_at("code"), Some(TOOL_NOT_STARTED));
    }

    #[test]
    fn started_tool_call_synthesizes_unknown_outcome() {
        let events = vec![
            event(0, "turn/start"),
            assistant_with_tool_calls(1, 0, 0, &["call-1"]),
            tool_call_event(2, 0, 0, "call-1"),
        ];
        let closers = closers_for(&events);
        assert_eq!(closers.len(), 2);
        let result = &closers[0];
        assert_eq!(result.event_type, "tool/result");
        assert_eq!(
            result.data.get("error").unwrap().as_str_at("code"),
            Some(TOOL_OUTCOME_UNKNOWN)
        );
        // The synthetic result cites the recorded tool/call seq.
        assert_eq!(result.source_event_seqs, vec![2]);
    }

    #[test]
    fn open_step_synthesizes_step_end_before_turn_end() {
        let events = vec![
            event(0, "turn/start"),
            step_start(1, 0, 0),
            assistant_with_tool_calls(2, 0, 0, &["call-1"]),
        ];
        let closers = closers_for(&events);
        assert_eq!(closers[0].event_type, "tool/result");
        assert_eq!(closers[1].event_type, "step/end");
        assert_eq!(closers[2].event_type, "turn/end");
    }

    #[test]
    fn resolved_tool_call_is_not_closed_again() {
        let events = vec![
            event(0, "turn/start"),
            assistant_with_tool_calls(1, 0, 0, &["call-1"]),
            tool_call_event(2, 0, 0, "call-1"),
            // A real result resolves the call; no synthetic result should follow.
            event(3, "tool/result"),
            event(4, "turn/end"),
        ];
        assert!(closers_for(&events).is_empty());
    }
}

// ── Round-6 regression: closers ride the model surface ──
#[cfg(test)]
mod round6_tests {
    use super::*;

    /// The closers exist so the resumed model transcript carries no dangling
    /// tool call — they must carry an Append surface op or the fold skips
    /// them (and the "repair" repairs nothing).
    #[test]
    fn closers_carry_an_append_surface_op() {
        let events = vec![
            super::tests::event(0, "turn/start"),
            super::tests::assistant_with_tool_calls(1, 0, 0, &["c1"]),
        ];
        let closers = closers_for(&events);
        assert!(closers.len() >= 1);
        assert!(
            matches!(
                closers[0].surface_op,
                Some(starriver_kernel::session::SurfaceOp::Append)
            ),
            "synthesized tool/result must be model-visible (got {:?})",
            closers[0].surface_op
        );
    }
}
