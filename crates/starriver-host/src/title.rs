//! LLM-generated session titles (provider source). The fallback title is the
//! truncated task text stamped at execution start; this module upgrades it to
//! a semantic title via a cheap auxiliary model call. User-pinned titles are
//! sacred — the whole pipeline refuses to touch them.

use starriver_kernel::types::Message;
use std::sync::Arc;

use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::{
    CallPurpose, ContentBlock, GenerateOptions, LlmRuntime, Message as KernelMessage, MessageId,
    MessageRole, MessageSource,
};

use crate::{HostRuntime, RuntimeInner};

/// Longest user text handed to the model as the naming subject.
const SUBJECT_BUDGET_CHARS: usize = 2000;
/// Assistant context head length when a second surface message exists.
const CONTEXT_HEAD_CHARS: usize = 200;

/// Build the auxiliary chat for a session title: a strict system instruction
/// plus the (truncated) conversation head. Mirrors the memory llm_select
/// precedent of a plain `[system, user]` call.
pub fn build_title_prompt(subject: &str, assistant_head: Option<&str>) -> Vec<Message> {
    let system = Message::system(
        "You name work sessions. Given the opening of a conversation, reply with ONLY a short \
         title: one line, at most 40 characters, no quotes, no prefix like \"Title:\". Reply in \
         the same language as the conversation.",
    );
    let mut user = format!(
        "Conversation opening:\n{}",
        clip(subject, SUBJECT_BUDGET_CHARS)
    );
    if let Some(head) = assistant_head {
        user.push_str(&format!(
            "\n\nAssistant began with:\n{}",
            clip(head, CONTEXT_HEAD_CHARS)
        ));
    }
    vec![system, Message::user(&user)]
}

/// Normalize a raw model reply into a usable title: shell quotes stripped,
/// first non-empty line only, whitespace collapsed at the edges, empty
/// results rejected, and the shared 40-char display cap applied.
pub fn clean_generated_title(raw: &str) -> Option<String> {
    // Models often open with a bare label line ("Title:" or its localized
    // form) — those
    let is_label_only = |line: &str| {
        let lowered = line.to_lowercase();
        lowered == "title"
            || lowered == "title:"
            || lowered == "标题"
            || lowered == "标题："
            || lowered == "标题:"
    };
    let line = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !is_label_only(line))?;
    // Drop wrapping shells like "…" / '…' / “…” around the whole line
    // (char-safe: quote marks may be multi-byte).
    let chars: Vec<char> = line.chars().collect();
    let unwrapped: &str = if chars.len() >= 2
        && ((chars[0] == '"' && *chars.last().unwrap() == '"')
            || (chars[0] == '\'' && *chars.last().unwrap() == '\'')
            || (chars[0] == '“' && *chars.last().unwrap() == '”'))
    {
        &line
            [line.chars().next().unwrap().len_utf8()..line.len() - chars.last().unwrap().len_utf8()]
    } else {
        line
    };
    let trimmed = unwrapped.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(crate::helpers::truncate_title(trimmed, 40))
}

fn clip(text: &str, max_chars: usize) -> String {
    // Same-label prefixes ("Title: xxx") are headers too — strip the label
    // so it does not leak into the generated title. The `…` counts toward
    // the budget: keep max_chars-1 chars + marker = max_chars total.
    let text = strip_same_line_label(text);
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Strip a leading "Title:" (or localized equivalent) label from the SAME line.
fn strip_same_line_label(text: &str) -> &str {
    for label in ["Title:", "title:", "标题：", "标题:"] {
        if let Some(rest) = text.strip_prefix(label) {
            return rest.trim_start();
        }
    }
    text
}

/// Title event payload with the provider source.
pub fn provider_title_event(title: &str) -> JsonValue {
    JsonValue::Object(vec![
        ("title".into(), JsonValue::String(title.into())),
        ("source".into(), JsonValue::String("provider".into())),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_carries_subject_and_optional_context() {
        let msgs = build_title_prompt("fix the parser", Some("I will start by"));
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].content_text().contains("40 characters"));
        assert!(msgs[1].content_text().contains("fix the parser"));
        assert!(msgs[1]
            .content_text()
            .contains("Assistant began with:\nI will start by"));

        let bare = build_title_prompt("just a task", None);
        assert!(!bare[1].content_text().contains("Assistant began"));
    }

    #[test]
    fn prompt_clips_oversized_subject() {
        let long = "x".repeat(SUBJECT_BUDGET_CHARS + 500);
        let msgs = build_title_prompt(&long, None);
        let text = msgs[1].content_text();
        assert!(text.chars().count() < long.chars().count());
        assert!(text.ends_with('…'));
    }

    #[test]
    fn cleaner_strips_shells_and_caps_length() {
        assert_eq!(
            clean_generated_title("\"Fix parser\"").as_deref(),
            Some("Fix parser")
        );
        assert_eq!(
            clean_generated_title("'my title'").as_deref(),
            Some("my title")
        );
        assert_eq!(
            clean_generated_title("“中文标题”").as_deref(),
            Some("中文标题")
        );
        assert_eq!(
            clean_generated_title("Title:\nthe actual line\nrest ignored").as_deref(),
            Some("the actual line")
        );
        assert_eq!(clean_generated_title("   \t ").is_none(), true);
        assert_eq!(clean_generated_title("").is_none(), true);
        // truncate_title caps at 40 bytes then appends a literal "...".
        let long = clean_generated_title(&"y".repeat(80)).unwrap();
        assert!(long.starts_with(&"y".repeat(40)));
        assert!(long.ends_with("..."));
        assert_eq!(long.len(), 43);
    }

    /// Text helper over the legacy content shape (plain text blocks only).
    trait ContentText {
        fn content_text(&self) -> String;
    }
    impl ContentText for Message {
        fn content_text(&self) -> String {
            self.content
                .iter()
                .filter_map(|b| match b {
                    starriver_kernel::types::ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

// ── Generation entry point ──

use crate::sessions::{
    append_session_event, ensure_durable, flatten_blocks, load_surface_messages,
};

impl HostRuntime {
    /// Generate and persist a provider-sourced session title from the first
    /// user message (plus a short assistant context head). Returns `Ok(None)`
    /// when nothing was generated — no active route, a user-pinned title, an
    /// unreadable session, or a failed/timed-out call; failures only warn.
    pub fn generate_session_title(
        &self,
        session_id: String,
        provider: String,
        model: String,
    ) -> anyhow::Result<Option<String>> {
        let inner = self.inner.clone();
        let handle = self.runtime_handle.clone();
        let result = std::thread::spawn(move || {
            handle.block_on(async {
                generate_title_inner(&inner, &session_id, &provider, &model).await
            })
        })
        .join()
        .map_err(|_| anyhow::anyhow!("thread panicked"))?;
        Ok(result)
    }
}

type SharedInner = tokio::sync::Mutex<Option<RuntimeInner>>;

/// Fire-and-forget post-turn title upgrade: detached task, warn-only
/// failures, called from the production execute path after the turn settles.
/// The user pin and all miss paths degrade to a skip inside. A free function
/// so the execute task (which only borrows `&HostRuntime`) can pass clones.
pub(crate) fn spawn_detached(
    inner: Arc<SharedInner>,
    handle: tokio::runtime::Handle,
    session_id: String,
    provider: String,
    model: String,
) {
    handle.spawn(async move {
        let generated = generate_title_inner(&inner, &session_id, &provider, &model).await;
        if generated.is_none() {
            tracing::debug!(session = %session_id, "background title upgrade skipped");
        }
    });
}

/// Pure builder for the audit payload (unit-tested): model hint (or null),
/// the subject user-message seq, and any assistant context seqs.
fn title_request_event_data(
    model_hint: &Option<String>,
    user_seq: u64,
    assistant_seqs: &[u64],
) -> starriver_kernel::json::JsonValue {
    use starriver_kernel::json::JsonValue;
    JsonValue::Object(vec![
        (
            "model".into(),
            match model_hint {
                Some(model) => JsonValue::String(model.clone()),
                None => JsonValue::Null,
            },
        ),
        (
            "subjectSeqs".into(),
            JsonValue::Array(vec![JsonValue::UInt(user_seq)]),
        ),
        (
            "assistantSeqs".into(),
            JsonValue::Array(
                assistant_seqs
                    .iter()
                    .copied()
                    .map(JsonValue::UInt)
                    .collect(),
            ),
        ),
    ])
}

async fn generate_title_inner(
    inner: &SharedInner,
    session_id: &str,
    provider: &str,
    model: &str,
) -> Option<String> {
    let guard = inner.lock().await;
    let runtime = guard.as_ref()?;
    if provider.is_empty() || model.is_empty() {
        tracing::debug!(session = %session_id, "title generation: no active route");
        return None;
    }
    let llm = runtime.agent.llm();
    if let Err(error) = ensure_durable(runtime).await {
        tracing::warn!(session = %session_id, %error, "title generation: flush failed");
        return None;
    }

    // 1) User pin is sacred.
    if let Some(row) = runtime.host_store.get(session_id) {
        if row.title_source.as_deref() == Some("user") {
            tracing::debug!(session = %session_id, "title pinned by user; skipping generation");
            return None;
        }
    }
    // 1b) A provider title already landed — keep it. Re-generating on every
    // turn end (subject is always the FIRST user message, model is
    // non-deterministic) made the sidebar title churn across the session.
    if let Some(row) = runtime.host_store.get(session_id) {
        if row.title_source.as_deref() == Some("provider") {
            tracing::debug!(session = %session_id, "provider title already set; skipping regeneration");
            return None;
        }
    }

    // 2) Subject material from the model-visible surface.
    let pairs = match load_surface_messages(runtime, session_id).await {
        Ok(pairs) => pairs,
        Err(error) => {
            tracing::warn!(session = %session_id, %error, "title generation: unreadable log");
            return None;
        }
    };
    let subject = pairs
        .iter()
        .find(|(_, message)| message.role == starriver_kernel::llm::MessageRole::User)
        .map(|(_, message)| flatten_blocks(&message.content))?;
    if subject.trim().is_empty() {
        return None;
    }
    let assistant_head = pairs
        .iter()
        .find(|(_, message)| message.role == starriver_kernel::llm::MessageRole::Assistant)
        .map(|(_, message)| flatten_blocks(&message.content));

    // SP4: log-only audit trail before any model involvement (even when
    // no engine resolves, the request attempt is on record).
    let user_seq = pairs
        .iter()
        .find(|(_, m)| m.role == starriver_kernel::llm::MessageRole::User)
        .map(|(seq, _)| *seq)
        .unwrap_or(0);
    let assistant_seqs: Vec<u64> = pairs
        .iter()
        .filter(|(_, m)| m.role == starriver_kernel::llm::MessageRole::Assistant)
        .map(|(seq, _)| *seq)
        .collect();
    let data = title_request_event_data(&Some(model.to_string()), user_seq, &assistant_seqs);
    if let Err(error) =
        append_session_event(runtime, session_id, "session/title-llm-request", data).await
    {
        tracing::warn!(session = %session_id, %error, "title request audit write failed");
    }

    // 3) Route + bounded auxiliary call through the kernel seam
    //    (`LlmRuntime::chat`, thinking disabled for session titles).
    let prompt_messages = build_title_prompt(&subject, assistant_head.as_deref());
    let text_of = |message: &Message| {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                starriver_kernel::types::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>()
    };
    let system = prompt_messages.first().map(&text_of);
    let user_text = prompt_messages.get(1).map(&text_of).unwrap_or_default();
    let options = GenerateOptions {
        provider: provider.to_string(),
        model: model.to_string(),
        reasoning_effort: None,
        tools: None,
        messages: vec![KernelMessage {
            id: MessageId("title-user".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::Text(user_text)],
            source: MessageSource::User,
        }],
        system,
        temperature: None,
        max_tokens: Some(256),
        stop: None,
        session_id: Some(session_id.to_string()),
        agent_id: None,
        tool_choice: None,
        cancellation: None,
        purpose: Some(CallPurpose::SessionTitle),
        loop_assembled: false,
    };
    let reply =
        match tokio::time::timeout(std::time::Duration::from_secs(30), llm.chat(options)).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(error)) => {
                tracing::warn!(session = %session_id, error = %error, "title generation failed");
                return None;
            }
            Err(_) => {
                tracing::warn!(session = %session_id, "title generation timed out");
                return None;
            }
        };

    // 4) Clean + write back over the shared meta-event channel.
    let Some(title) = clean_generated_title(&reply) else {
        tracing::warn!(session = %session_id, "title generation produced an empty reply");
        return None;
    };
    if crate::host_store::HostStore::is_reserved_group_name(&title) {
        tracing::warn!(session = %session_id, title = %title, "generated title collides with a reserved sidebar name; skipped");
        return None;
    }
    if let Err(error) = append_session_event(
        runtime,
        session_id,
        "session/title",
        provider_title_event(&title),
    )
    .await
    {
        tracing::warn!(session = %session_id, %error, "provider title write failed");
        return None;
    }
    Some(title)
}

#[cfg(test)]
mod sp4_payload_tests {
    use super::*;

    #[test]
    fn audit_payload_carries_model_hint_and_seqs() {
        let data = title_request_event_data(&Some("deepseek-v4".into()), 7, &[9, 11]);
        assert_eq!(data.as_str_at("model"), Some("deepseek-v4"));
        let seqs = data
            .get("subjectSeqs")
            .and_then(starriver_kernel::json::JsonValue::as_array)
            .unwrap();
        assert_eq!(seqs.len(), 1);
        assert_eq!(seqs[0].as_u64(), Some(7));
        let assistants = data
            .get("assistantSeqs")
            .and_then(starriver_kernel::json::JsonValue::as_array)
            .unwrap();
        let nums: Vec<u64> = assistants
            .iter()
            .filter_map(starriver_kernel::json::JsonValue::as_u64)
            .collect();
        assert_eq!(nums, vec![9, 11]);
    }

    #[test]
    fn audit_payload_null_model_without_hint() {
        let data = title_request_event_data(&None, 3, &[]);
        let is_null = matches!(
            data.get("model"),
            Some(starriver_kernel::json::JsonValue::Null)
        );
        assert!(is_null);
        let assistants = data
            .get("assistantSeqs")
            .and_then(starriver_kernel::json::JsonValue::as_array)
            .unwrap();
        assert!(assistants.is_empty());
    }
}
