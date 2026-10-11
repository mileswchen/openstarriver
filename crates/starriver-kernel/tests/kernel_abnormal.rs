//! Kernel abnormal-path integration tests: cross-module flows through the
//! public session / wire / json / agent-seam APIs.

use cordis_rslite::EventBus;
use starriver_kernel::agent::{Inbox, InboxNotifications, InboxTarget};
use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::{
    ContentBlock, ContentBlockType, FinishReason, LlmFailure, Message, MessageId, MessageRole,
    MessageSource, ReplayEnvelope, StreamChunk, TokenUsage,
};
use starriver_kernel::session::{
    AppendOptions, Session, SessionError, SessionId, SurfaceOp, TurnEndReason,
};

fn user_message(text: &str) -> Message {
    Message {
        id: MessageId(text.into()),
        role: MessageRole::User,
        content: vec![ContentBlock::Text(text.into())],
        source: MessageSource::User,
    }
}

#[test]
fn surface_derive_projects_wire_messages_and_rejects_malformed() {
    let bus = EventBus::new();
    let session = Session::new(SessionId("s1".into()), bus);
    let user = user_message("hi");
    session
        .append_with(
            "user/message",
            user.to_json().unwrap(),
            AppendOptions {
                surface_op: Some(SurfaceOp::Append),
                source_event_seqs: Vec::new(),
            },
        )
        .unwrap();
    assert_eq!(session.derive_messages().unwrap().len(), 1);

    // A malformed surface payload must surface as a structured error and must
    // not corrupt the log.
    session
        .append_with(
            "assistant/message",
            JsonValue::Object(Vec::new()),
            AppendOptions {
                surface_op: Some(SurfaceOp::Append),
                source_event_seqs: Vec::new(),
            },
        )
        .unwrap();
    let error = session.derive_messages().unwrap_err();
    assert!(matches!(error, SessionError::Surface(_)));
    assert_eq!(session.events().len(), 2, "the log itself stays intact");
}

#[test]
fn surface_replace_shadows_prior_nodes() {
    let bus = EventBus::new();
    let session = Session::new(SessionId("s1".into()), bus);
    for text in ["a", "b"] {
        session
            .append_with(
                "user/message",
                user_message(text).to_json().unwrap(),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: Vec::new(),
                },
            )
            .unwrap();
    }
    let replacement = user_message("compacted");
    session
        .append_with(
            "user/message",
            replacement.to_json().unwrap(),
            AppendOptions {
                surface_op: Some(SurfaceOp::Replace { start: 0, end: 1 }),
                source_event_seqs: Vec::new(),
            },
        )
        .unwrap();

    let fold = session.surface();
    assert_eq!(fold.nodes, vec![2]);
    assert_eq!(fold.replacements.len(), 1);
    assert_eq!(fold.replacements[0].shadowed_seqs, vec![0, 1]);
    let messages = session.derive_messages().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id.0, "compacted");
}

#[test]
fn inbox_replay_matches_and_duplicate_panic_leaves_log_clean() {
    let bus = EventBus::new();
    let session = Session::new(SessionId("s1".into()), bus);
    let mut inbox = Inbox::new(session.clone(), InboxNotifications::silent());
    inbox.append(InboxTarget::NextTurn, user_message("a"));
    inbox.append(InboxTarget::NextStep, user_message("b"));
    inbox.remove(&MessageId("a".into()));

    // A fresh projection replays the durable splices.
    let replayed = Inbox::new(session.clone(), InboxNotifications::silent());
    assert!(replayed.next_turn().is_empty());
    assert_eq!(replayed.next_step().len(), 1);
    assert_eq!(replayed.next_step()[0].id.0, "b");

    // A duplicate (cross-queue) insertion panics before any durable event.
    let before = session.events().len();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        inbox.append(InboxTarget::NextTurn, user_message("b"));
    }));
    assert!(result.is_err());
    assert_eq!(
        session.events().len(),
        before,
        "failed splice must not write"
    );
}

#[test]
fn stream_chunk_wire_roundtrips_through_session_log_and_rejects_malformed() {
    let bus = EventBus::new();
    let session = Session::new(SessionId("s1".into()), bus);
    let chunk = StreamChunk::BlockStart {
        index: 0,
        block_type: ContentBlockType::Custom("video".into()),
    };
    let json = chunk.to_json().unwrap();
    session
        .append_with(
            "assistant/chunk",
            JsonValue::Object(vec![("chunk".into(), json)]),
            AppendOptions::default(),
        )
        .unwrap();
    let stored = session.events()[0].data.get("chunk").unwrap().clone();
    let back = StreamChunk::from_json(stored).unwrap();
    assert_eq!(format!("{back:?}"), format!("{chunk:?}"));

    // Malformed wire inputs are rejected cleanly.
    assert!(StreamChunk::from_json(JsonValue::String("x".into())).is_err());
    assert!(ContentBlock::from_json(JsonValue::String("x".into())).is_err());
    assert!(Message::from_json(JsonValue::Object(Vec::new())).is_err());
}

#[test]
fn turn_end_reason_roundtrips_through_session_log() {
    use starriver_kernel::agent::{AgentCancelCause, AgentCancelKind};

    let bus = EventBus::new();
    let session = Session::new(SessionId("s1".into()), bus);
    session
        .append_with(
            "turn/end",
            JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(1)),
                (
                    "reason".into(),
                    TurnEndReason::Aborted {
                        cause: AgentCancelCause::new(AgentCancelKind::Parent),
                    }
                    .to_json(),
                ),
            ]),
            AppendOptions::default(),
        )
        .unwrap();
    let events = session.events();
    let reason = events[0].data.get("reason").unwrap();
    assert_eq!(reason.as_str_at("kind"), Some("aborted"));
    assert_eq!(reason.as_str_at("cause"), Some("parent"));

    let failure = LlmFailure {
        message: "boom".into(),
        code: "SERVER".into(),
        status: Some(500),
        provider_retry_after_ms: None,
        request_id: None,
    };
    let json = TurnEndReason::Error {
        failure: failure.clone(),
    }
    .to_json();
    assert_eq!(json.as_str_at("kind"), Some("error"));
    assert_eq!(
        json.get("failure").and_then(|f| f.as_str_at("code")),
        Some("SERVER")
    );
}

#[test]
fn finish_reason_with_replay_envelope_roundtrips() {
    let failure = LlmFailure {
        message: "boom".into(),
        code: "SERVER".into(),
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    };
    let chunk = StreamChunk::Finish {
        reason: FinishReason::Error { failure },
        replay_state: Some(ReplayEnvelope {
            response: JsonValue::Object(vec![("k".into(), JsonValue::Int(1))]),
            blocks: Some(vec![JsonValue::String("b".into())]),
        }),
    };
    let json = chunk.to_json().unwrap();
    let back = StreamChunk::from_json(json).unwrap();
    assert_eq!(format!("{back:?}"), format!("{chunk:?}"));

    let usage = StreamChunk::Usage {
        usage: TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: Some(3),
            cache_write_tokens: None,
            reasoning_tokens: Some(4),
        },
    };
    let back = StreamChunk::from_json(usage.to_json().unwrap()).unwrap();
    assert_eq!(format!("{back:?}"), format!("{usage:?}"));
}
