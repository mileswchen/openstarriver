//! Cold recovery: synthetic closers with tool placeholders, and loud refusal
//! on committed-prefix corruption.

use rusqlite::Connection;
use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::{CallId, ContentBlock, Message, MessageId, MessageRole, MessageSource};
use starriver_kernel::session::{SessionEvent, SessionHeader, SessionId};
use starriver_session_persistence::{repair, schema, store};

fn header(id: &str) -> SessionHeader {
    let mut h = SessionHeader::new(SessionId(id.into()));
    h.cwd = Some("/tmp".into());
    h
}

fn event(seq: u64, event_type: &str) -> SessionEvent {
    SessionEvent {
        seq,
        time: 1,
        event_type: event_type.into(),
        data: JsonValue::Null,
        surface_op: None,
        source_event_seqs: Vec::new(),
    }
}

fn assistant_with_tool_call(seq: u64, call_id: &str) -> SessionEvent {
    let message = Message {
        id: MessageId(format!("m{seq}")),
        role: MessageRole::Assistant,
        content: vec![ContentBlock::ToolCall {
            id: CallId(call_id.to_string()),
            name: Some("echo".into()),
            arguments: JsonValue::Object(Vec::new()),
        }],
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
            ("turn".into(), JsonValue::UInt(0)),
            ("step".into(), JsonValue::UInt(0)),
            ("message".into(), message.to_json().unwrap()),
        ]),
        surface_op: None,
        source_event_seqs: Vec::new(),
    }
}

fn tool_call(seq: u64, call_id: &str) -> SessionEvent {
    SessionEvent {
        seq,
        time: 1,
        event_type: "tool/call".into(),
        data: JsonValue::Object(vec![
            ("turn".into(), JsonValue::UInt(0)),
            ("step".into(), JsonValue::UInt(0)),
            ("callId".into(), JsonValue::String(call_id.into())),
            ("name".into(), JsonValue::String("echo".into())),
            ("arguments".into(), JsonValue::String("{}".into())),
        ]),
        surface_op: None,
        source_event_seqs: Vec::new(),
    }
}

#[test]
fn repair_synthesizes_tool_placeholders_for_open_turn() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let mut db = Connection::open(&path).unwrap();
    schema::open_owned(&db, &path).unwrap();
    let id = SessionId("s1".into());
    let h = header("s1");
    // Open turn: assistant requested a tool, no result yet.
    store::append_batch(
        &mut db,
        &h,
        &[
            event(0, "turn/start"),
            event(1, "user/message"),
            assistant_with_tool_call(2, "call-1"),
            tool_call(3, "call-1"),
        ],
        false,
    )
    .unwrap();
    let stored = store::load_stored(&db, "sid", &id).unwrap().unwrap();
    let closers = repair::closers_for(&stored.events);
    let types = closers
        .iter()
        .map(|e| e.event_type.as_str())
        .collect::<Vec<_>>();
    assert!(types.contains(&"tool/result"));
    assert!(types.contains(&"turn/end"));
    let tool_result = closers
        .iter()
        .find(|e| e.event_type == "tool/result")
        .unwrap();
    assert_eq!(
        tool_result
            .data
            .get("error")
            .and_then(|e| e.as_str_at("code")),
        Some(repair::TOOL_OUTCOME_UNKNOWN)
    );
}

#[test]
fn corruption_in_committed_prefix_rejects_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let mut db = Connection::open(&path).unwrap();
    schema::open_owned(&db, &path).unwrap();
    let id = SessionId("s1".into());
    let h = header("s1");
    store::append_batch(
        &mut db,
        &h,
        &[event(0, "turn/start"), event(1, "user/message")],
        false,
    )
    .unwrap();
    // A corrupt row in the committed prefix (NOT the tail) must reject.
    db.execute("UPDATE events SET data = '{broken' WHERE seq = 0", [])
        .unwrap();
    let result = store::load_stored(&db, "sid", &id);
    assert!(result.is_err());
}

#[test]
fn torn_tail_is_truncated_not_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let mut db = Connection::open(&path).unwrap();
    schema::open_owned(&db, &path).unwrap();
    let id = SessionId("s1".into());
    let h = header("s1");
    store::append_batch(
        &mut db,
        &h,
        &[event(0, "turn/start"), event(1, "user/message")],
        false,
    )
    .unwrap();
    // A torn write leaves the LAST row physically corrupt. The committed
    // prefix (seq 0) is intact; the tail (seq 1) must be truncated, not
    // rejected as corruption.
    db.execute("UPDATE events SET data = '{torn' WHERE seq = 1", [])
        .unwrap();
    let stored = store::load_stored(&db, "sid", &id).unwrap().unwrap();
    assert_eq!(stored.events.len(), 1);
    assert_eq!(stored.events[0].seq, 0);
}
