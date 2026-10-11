//! WI-6 `session_reference`: pull a folded summary of another session's
//! surface into this conversation on demand. Read-only: the tool loads the
//! target session's event log through the host persistence layer and
//! projects it to compact `[role] excerpt` lines (newest-first selection,
//! chronological output, char budget ≈ 8K tokens).

use std::sync::Arc;

use cordis_rslite::Value;
use starriver_kernel::json::JsonValue;
use starriver_kernel::session::SessionId;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
use starriver_session_persistence::SqliteSessionPersistence;

use super::common::{invalid_input, json_args, text_content, tool_failure};
use crate::sessions::flatten_blocks;

/// Default cap on projected messages per call.
pub const DEFAULT_MAX_MESSAGES: u32 = 40;
/// Projection budget ≈ 8K tokens (chars/4).
pub const BUDGET_CHARS: usize = 32 * 1024;
/// Per-message excerpt length.
const EXCERPT_CHARS: usize = 200;

struct SessionReference {
    persistence: Arc<SqliteSessionPersistence>,
}

impl ToolExecutor for SessionReference {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        // Cloned up front: the boxed future is 'static and must not borrow
        // `&self`.
        let persistence = Arc::clone(&self.persistence);
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let session_id = json
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| invalid_input("session_reference", "missing session_id".into()))?;
            let query = json
                .get("query")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let max_messages = json
                .get("max_messages")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(DEFAULT_MAX_MESSAGES as usize)
                .clamp(1, 500);

            // The load touches SQLite — keep it off the async worker.
            let sid = session_id.to_string();
            let result = tokio::task::spawn_blocking(move || {
                project_session(&persistence, &sid, query.as_deref(), max_messages)
            })
            .await
            .map_err(|e| tool_failure("session_reference", "JOIN_FAILED", format!("{e}")))?;
            match result {
                Ok(text) => Ok(Value::new(serde_json::json!({ "output": text }))),
                Err((code, message)) => Err(tool_failure("session_reference", &code, message)),
            }
        })
    }
}

/// Pure projection: chronological `[role] excerpt` lines for a session's
/// event log (real user/assistant/tool-result messages only; plugin plumbing
/// skipped). `query` filters case-insensitively.
pub(crate) fn project_reference_lines(
    events: &[starriver_kernel::session::SessionEvent],
    query: Option<&str>,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for event in events {
        let role = match event.event_type.as_str() {
            "user/message" => "user",
            "assistant/message" => "assistant",
            "tool/result" => "tool",
            _ => continue,
        };
        // user/message carries the bare message; assistant/tool results wrap
        // theirs under `message`.
        let data = if role == "user" {
            event.data.clone()
        } else {
            event
                .data
                .get("message")
                .cloned()
                .unwrap_or(JsonValue::Null)
        };
        let Ok(message) = starriver_kernel::llm::Message::from_json(data) else {
            continue;
        };
        if role == "user"
            && matches!(
                message.source,
                starriver_kernel::llm::MessageSource::Plugin { .. }
            )
        {
            continue; // model-visible plumbing, not real user input
        }
        let text = flatten_blocks(&message.content);
        if text.trim().is_empty() {
            continue;
        }
        let line = format!("[{role}] {}", collapse_lines(&text));
        if let Some(q) = query {
            let q_lower = q.to_ascii_lowercase();
            if !line.to_ascii_lowercase().contains(&q_lower) {
                continue;
            }
        }
        lines.push(line);
    }
    lines
}

/// Load one session's log and project it under the char budget
/// (newest-first selection, chronological output).
fn project_session(
    persistence: &SqliteSessionPersistence,
    session_id: &str,
    query: Option<&str>,
    max_messages: usize,
) -> Result<String, (&'static str, String)> {
    let inspection = persistence
        .coordinator()
        .load(SessionId(session_id.to_string()))
        .map_err(|e| ("NOT_FOUND", format!("session unavailable: {e}")))?;
    let lines = project_reference_lines(&inspection.events, query);
    let mut selected: Vec<String> = Vec::new();
    let mut budget = BUDGET_CHARS;
    for line in lines.iter().rev() {
        if selected.len() >= max_messages || budget == 0 {
            break;
        }
        let excerpt: String = line.chars().take(EXCERPT_CHARS).collect();
        // The budget is CHAR-denominated like the excerpt cut (BUG-100:
        // byte-cost let CJK excerpts blow BUDGET_CHARS by ~3x).
        let cost = excerpt.chars().count().min(budget);
        budget -= cost;
        let excerpt: String = if cost < excerpt.chars().count() { excerpt.chars().take(cost).collect() } else { excerpt };
        selected.push(excerpt);
    }
    selected.reverse();
    if selected.is_empty() {
        return Ok(format!(
            "## 会话 {session_id}\n(预算内没有匹配的消息;共 {} 条可见消息)",
            lines.len()
        ));
    }
    Ok(format!(
        "## 会话 {session_id} — 引用 {} 条(会话共 {} 条可见消息)\n{}",
        selected.len(),
        lines.len(),
        selected.join("\n")
    ))
}

fn collapse_lines(text: &str) -> String {
    text.split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ⏎ ")
}

/// The model-facing description text (extracted for contract tests).
pub(crate) fn session_reference_description_text() -> &'static str {
    "Quote another conversation's content into this one. Parameters: \
     session_id (the other session's id), optional query (keyword filter) \
     and max_messages. Returns compact `[role] excerpt` lines (selected \
     newest-first, rendered chronologically); fails with NOT_FOUND \
     when the session does not exist."
}

pub fn session_reference_definition(persistence: Arc<SqliteSessionPersistence>) -> ToolDefinition {
    ToolDefinition {
        name: "session_reference".into(),
        description: session_reference_description_text().into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "id of the session to reference" },
                "query": { "type": "string", "description": "optional keyword filter over message text" },
                "max_messages": { "type": "integer", "description": "cap on projected messages (default 40)" }
            },
            "required": ["session_id"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(SessionReference { persistence }),
        is_concurrency_safe: None,
        timeout: None,
        finalize_content: None,
        // Read-only family: it never mutates anything, so no approval and no
        // read-only refusal.
        requires_approval: false,
        deny_in_readonly: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::json::JsonValue;
    use starriver_kernel::llm::{
        CallId, ContentBlock, Message, MessageId, MessageRole, MessageSource,
    };
    use starriver_kernel::tools::ToolRuntime;

    fn wrap(data: JsonValue) -> JsonValue {
        JsonValue::Object(vec![("message".into(), data)])
    }

    fn msg_event(event_type: &str, message: Message) -> starriver_kernel::session::SessionEvent {
        let data = if event_type == "user/message" {
            message.to_json().unwrap()
        } else {
            wrap(message.to_json().unwrap())
        };
        starriver_kernel::session::SessionEvent {
            seq: 0,
            time: 0,
            event_type: event_type.into(),
            data,
            surface_op: None,
            source_event_seqs: Vec::new(),
        }
    }

    fn text_message(role: MessageRole, source: MessageSource, text: &str) -> Message {
        Message {
            id: MessageId("m".into()),
            role,
            content: vec![ContentBlock::Text(text.to_string())],
            source,
        }
    }

    #[test]
    fn projection_covers_real_messages_and_skips_plumbing() {
        let events = vec![
            msg_event(
                "user/message",
                text_message(
                    MessageRole::User,
                    MessageSource::User,
                    "帮我修 sqlite 的锁问题",
                ),
            ),
            // Plugin plumbing (runtime-context snapshot) must be skipped.
            msg_event(
                "user/message",
                text_message(
                    MessageRole::User,
                    MessageSource::Plugin {
                        plugin: "runtime-context".into(),
                        form: None,
                    },
                    "Current runtime context …",
                ),
            ),
            msg_event(
                "assistant/message",
                text_message(
                    MessageRole::Assistant,
                    MessageSource::Model {
                        provider: "p".into(),
                        model: "m".into(),
                        replay_state: None,
                    },
                    "定位到 busy 等待,建议开 WAL",
                ),
            ),
        ];
        let lines = super::project_reference_lines(&events, None);
        assert_eq!(lines.len(), 2, "plumbing skipped: {lines:?}");
        assert!(lines[0].starts_with("[user]"), "{lines:?}");
        assert!(lines[1].starts_with("[assistant]"), "{lines:?}");
    }

    #[test]
    fn query_filter_keeps_only_matching_lines() {
        let events = vec![
            msg_event(
                "user/message",
                text_message(MessageRole::User, MessageSource::User, "关于 rust 编译"),
            ),
            msg_event(
                "assistant/message",
                text_message(
                    MessageRole::Assistant,
                    MessageSource::Model {
                        provider: "p".into(),
                        model: "m".into(),
                        replay_state: None,
                    },
                    "聊聊周末计划",
                ),
            ),
        ];
        let lines = super::project_reference_lines(&events, Some("rust"));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("rust"));
    }

    fn temp_persistence() -> (tempfile::TempDir, Arc<SqliteSessionPersistence>) {
        let dir = tempfile::tempdir().unwrap();
        let persistence =
            Arc::new(SqliteSessionPersistence::open(dir.path().join("sessions.db")).unwrap());
        (dir, persistence)
    }

    #[tokio::test]
    async fn definition_shape_matches_tool_family() {
        let (_dir, persistence) = temp_persistence();
        let def = session_reference_definition(persistence);
        assert_eq!(def.name, "session_reference");
        assert!(!def.requires_approval, "read-only family: no approval");
        assert!(!def.deny_in_readonly, "read-only family: allowed in RO");
    }

    #[tokio::test]
    async fn registry_contains_session_reference_schema() {
        let (_dir, persistence) = temp_persistence();
        let runtime = starriver_tools::InMemoryToolRuntime::new(
            cordis_rslite::EventBus::new(),
            cordis_rslite::Context::new(),
        );
        let _ = runtime
            .register(session_reference_definition(persistence))
            .unwrap();
        let schema = runtime
            .schemas()
            .into_iter()
            .find(|schema| schema.name == "session_reference")
            .expect("session_reference must appear in the registry schemas");
        assert!(schema.parameters.get("required").is_some());
    }
}


// ── BUG-98/100 regressions (round 6) ──
#[cfg(test)]
mod round6_tests {
    use super::*;

    #[test]
    fn tool_description_matches_chronological_output() {
        let def_text = session_reference_description_text();
        assert!(
            def_text.contains("chronologically"),
            "the model-facing contract must match the actual (chronological) output: {def_text}"
        );
    }
}
