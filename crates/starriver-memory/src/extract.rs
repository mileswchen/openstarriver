// Extraction & selection passes — the LLM-facing side of the memory system.
// All prompts/parse helpers operate on this crate's `ChatMessage` mirror so no
// core/kernel types leak in.

use futures::future::BoxFuture;

use crate::chat::{ChatBlock, ChatMessage, ChatRole};
use crate::store::{MemoryEntry, MemoryStore, GLOBAL_PROJECT};

/// Draft extracted by the background LLM pass — the shape of one JSON array
/// element the model must return. project/timestamps are filled in later.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MemoryEntryDraft {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// Extract the "worth remembering" slice of a conversation: only `User` and
/// `Assistant` plain-text blocks. Tool results / thinking / tool-use are skipped —
/// the redaction red line (secrets and paths in bash output never become memory).
pub fn extractable_text(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        let label = match m.role {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
            _ => continue,
        };
        let text: Vec<&str> = m
            .content
            .iter()
            .filter_map(|b| match b {
                ChatBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        if text.is_empty() {
            continue;
        }
        out.push_str(&format!("[{label}] {}\n", text.join(" ")));
    }
    out
}

/// Prompt for the background extraction pass. The model replies with ONLY a JSON
/// array of `MemoryEntryDraft`s.
///
/// The transient-policy red line exists because of a real failure: a plan-mode
/// (read-only sandbox) session hit write refusals, and the extractor persisted
/// "sandbox is read-only; file writes must go through API X" as a durable fact
/// — every later session then recalled the wrong constraint. `parse_extract_json`
/// output is additionally screened by `is_transient_policy_fact`.
pub fn extract_prompt(conversation: &str) -> Vec<ChatMessage> {
    let system = ChatMessage::system(
        "You extract durable, cross-session facts from a coding-agent conversation. \
         Keep ONLY facts reusable in future sessions: user preferences, project conventions, \
         environment constraints, and gotchas the team must not repeat. Ignore one-off task \
         details and transient state. Sandbox, permission, approval, and execution-mode states \
         (read-only vs writable, manual vs auto approval, plan mode) are transient session \
         configuration — NEVER store them as durable facts, and never store a conclusion drawn \
         from a tool refusal or permission error (e.g. \"the sandbox is read-only\", \
         \"file writes are refused\", \"writes must go through API X\"). \
         Respond with ONLY a JSON array, no markdown, of objects \
         shaped {\"name\":\"<english slug>\",\"description\":\"<one line>\",\"body\":\"<1-3 sentence fact>\"}. \
         Use [] if nothing is worth remembering.",
    );
    let user = ChatMessage::user(&format!("Conversation:\n\n{conversation}"));
    vec![system, user]
}

/// Deterministic backstop for the red line above: true when a draft asserts a
/// transient sandbox/permission state as a durable fact. Applied to every
/// extracted draft before it reaches the store (deliberate `memory_write`
/// calls are unaffected). Normalizes case and `-`/`_` so slugs match too.
pub fn is_transient_policy_fact(name: &str, description: &str, body: &str) -> bool {
    let text = format!("{name} {description} {body}")
        .to_lowercase()
        .replace(['-', '_'], " ");
    // "the sandbox is read-only" / "read-only sandbox" / slug
    // "sandbox-readonly-write-via-codeup-api".
    let read_only_sandbox =
        text.contains("sandbox") && (text.contains("read only") || text.contains("readonly"));
    // "file-writing tools are refused" / "writes are denied" / "writes blocked".
    let write_word =
        ["write", "writes", "writing"].iter().any(|w| text.contains(w));
    let refusal_word = [
        "refus",
        "deni",
        "forbid",
        "not permitted",
        "blocked",
    ]
    .iter()
    .any(|w| text.contains(w));
    read_only_sandbox || (write_word && refusal_word)
}

/// Parse the model's JSON array reply (tolerating leading/trailing prose
/// and trailing commas — a deterministic parse failure here would otherwise
/// re-bill the same session on every sweep, per the BUG-70 no-advance rule).
pub fn parse_extract_json(text: &str) -> Result<Vec<MemoryEntryDraft>, String> {
    extract_json_array(text).and_then(|slice| {
        serde_json::from_str(&strip_trailing_commas(slice))
            .map_err(|e| format!("invalid extract JSON: {e}"))
    })
}

/// Remove `,` that directly precedes `]` or `}` (outside strings) — the most
/// common malformed-JSON habit of extraction models.
fn strip_trailing_commas(slice: &str) -> String {
    let mut out = String::with_capacity(slice.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut pending_commas = String::new();
    for ch in slice.chars() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                out.push_str(&pending_commas);
                pending_commas.clear();
                in_string = true;
                out.push(ch);
            }
            ']' | '}' => {
                pending_commas.clear();
                out.push(ch);
            }
            ',' => pending_commas.push(','),
            c if c.is_whitespace() && !pending_commas.is_empty() => pending_commas.push(c),
            c => {
                out.push_str(&pending_commas);
                pending_commas.clear();
                out.push(c);
            }
        }
    }
    out
}

/// Prompt for the opt-in LLM selection pass: the model picks which stored
/// memories are relevant to a new request, returning their slugs.
pub fn build_selection_prompt(entries: &[MemoryEntry], query: &str) -> Vec<ChatMessage> {
    let mut idx = String::new();
    for e in entries {
        idx.push_str(&format!("- {}: {}\n", e.name, e.description));
    }
    let system = ChatMessage::system(
        "You select which stored memories are relevant to a new user request. \
         Given a memory index (slug: one-line description) and the request, return ONLY a JSON \
         array of the relevant slugs (the part before ':'). Return [] if none are relevant.",
    );
    let user = ChatMessage::user(&format!("Request: {query}\n\nMemory index:\n{idx}"));
    vec![system, user]
}

/// Parse the model's slug-array reply (tolerating leading/trailing prose).
pub fn parse_selection_json(text: &str) -> Result<Vec<String>, String> {
    extract_json_array(text).and_then(|slice| {
        serde_json::from_str(slice).map_err(|e| format!("invalid selection JSON: {e}"))
    })
}

/// Shared bracket-scanner: locate the first balanced JSON array in `text`
/// (string-aware, so `]` inside strings does not close it).
fn extract_json_array(text: &str) -> Result<&str, String> {
    let start = text
        .find('[')
        .ok_or_else(|| "no JSON array found".to_string())?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut end = text.len();
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = start + offset + ch.len_utf8();
                    break;
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err("unbalanced array in LLM output".to_string());
    }
    Ok(&text[start..end])
}

/// Opt-in LLM selection: let the model pick relevant memories from the index,
/// then return their full entries. Falls back to empty on any failure.
pub async fn llm_select(
    store: &MemoryStore,
    project: &str,
    query: &str,
    chat: impl Fn(String) -> BoxFuture<'static, Result<String, String>> + Send,
) -> Vec<MemoryEntry> {
    let mut scopes = vec![project.to_string()];
    if project != GLOBAL_PROJECT {
        scopes.push(GLOBAL_PROJECT.to_string());
    }
    let mut entries = Vec::new();
    for scope in scopes {
        if let Ok(e) = store.list(&scope) {
            entries.extend(e);
        }
    }
    if entries.is_empty() {
        return Vec::new();
    }
    let prompt = build_selection_prompt(&entries, query);
    let prompt_text = prompt
        .iter()
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    ChatBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let Ok(reply) = chat(prompt_text).await else {
        return Vec::new();
    };
    let Ok(slugs) = parse_selection_json(&reply) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter(|e| slugs.contains(&e.name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MemoryConfig, MemorySource, DEFAULT_SOURCE_AGENT};

    fn entry(name: &str, desc: &str, project: &str, updated: i64) -> MemoryEntry {
        MemoryEntry {
            name: name.to_string(),
            description: desc.to_string(),
            body: format!("{desc} (body)"),
            created: updated,
            updated,
            source: MemorySource::Extract,
            source_agent: DEFAULT_SOURCE_AGENT.to_string(),
            project: project.to_string(),
        }
    }

    fn store() -> (tempfile::TempDir, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memory");
        let store = MemoryStore::new(root, MemoryConfig::default());
        (dir, store)
    }

    // ── extraction ──

    #[test]
    fn extractable_text_keeps_user_and_assistant() {
        let msgs = vec![ChatMessage::user("hi"), ChatMessage::assistant("done")];
        let out = extractable_text(&msgs);
        assert!(out.contains("[user] hi"));
        assert!(out.contains("[assistant] done"));
    }

    #[test]
    fn extractable_text_skips_tool_and_thinking() {
        let msgs = vec![
            ChatMessage::user("run it"),
            ChatMessage::assistant("on it"),
            ChatMessage {
                role: ChatRole::User,
                content: vec![ChatBlock::Other], // tool result
            },
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![ChatBlock::Thinking {
                    text: "internal reasoning".into(),
                }],
            },
        ];
        let out = extractable_text(&msgs);
        assert!(out.contains("[user] run it"));
        assert!(!out.contains("SECRET"), "tool result must be redacted");
        assert!(
            !out.contains("internal reasoning"),
            "thinking must be redacted"
        );
    }

    #[test]
    fn extractable_text_empty_on_no_text() {
        let msgs = vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ChatBlock::Other],
        }];
        assert_eq!(extractable_text(&msgs), "");
    }

    #[test]
    fn extract_prompt_has_system_and_user() {
        let p = extract_prompt("some conversation");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].role, ChatRole::System);
        assert_eq!(p[1].role, ChatRole::User);
    }

    #[test]
    fn extract_prompt_carries_transient_policy_red_line() {
        let p = extract_prompt("some conversation");
        let system: String = p[0]
            .content
            .iter()
            .filter_map(|b| match b {
                ChatBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(system.contains("transient session configuration"));
        assert!(system.contains("tool refusal"));
    }

    // ── transient policy backstop (game004 regression) ──

    #[test]
    fn transient_policy_fact_flags_the_real_world_memory() {
        // The exact entry extracted on 2026-09-12 in game004.
        assert!(is_transient_policy_fact(
            "sandbox-readonly-write-via-codeup-api",
            "Sandbox is read-only; file writes must go through 云效 Codeup open APIs.",
            "In this environment the sandbox policy is read-only and file-writing tools \
             are refused. The landable write path is the 云效 Codeup open API (create \
             repository + commit files), not local disk operations.",
        ));
    }

    #[test]
    fn transient_policy_fact_flags_variants() {
        for (name, desc, body) in [
            ("Read-Only Sandbox", "sandbox is read-only", "writes fail"),
            ("readonly-sandbox", "", "the sandbox is readonly here"),
            (
                "writes-refused",
                "File writes are refused by the environment",
                "use the API instead",
            ),
            (
                "plan-mode-no-write",
                "",
                "the write tool was denied, so save files via commits",
            ),
        ] {
            assert!(
                is_transient_policy_fact(name, desc, body),
                "must flag: {name}"
            );
        }
    }

    #[test]
    fn transient_policy_fact_keeps_durable_facts() {
        for (name, desc, body) in [
            ("uses-pnpm", "deps via pnpm", "This project uses pnpm."),
            (
                "ci-deploy-path",
                "deploys must go through the CI api",
                "never deploy from a laptop",
            ),
            (
                "main-branch-policy",
                "direct pushes to main are blocked",
                "open a PR instead",
            ),
            (
                "user-language-chinese",
                "User writes in Chinese",
                "expect Chinese responses",
            ),
        ] {
            assert!(
                !is_transient_policy_fact(name, desc, body),
                "must keep: {name}"
            );
        }
    }

    #[test]
    fn parse_extract_json_plain_array() {
        let json = r#"[{"name":"npm-uses-pnpm","description":"use pnpm","body":"deps via pnpm"}]"#;
        let drafts = parse_extract_json(json).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].name, "npm-uses-pnpm");
    }

    #[test]
    fn parse_extract_json_tolerates_prose() {
        let text = "Here are the facts:\n[{\"name\":\"a\",\"description\":\"d\",\"body\":\"b\"}]\nHope this helps.";
        let drafts = parse_extract_json(text).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].name, "a");
    }

    #[test]
    fn parse_extract_json_empty_array() {
        assert!(parse_extract_json("[]").unwrap().is_empty());
    }

    #[test]
    fn parse_extract_json_invalid_is_err() {
        assert!(parse_extract_json("no json here").is_err());
        assert!(parse_extract_json("[{\"name\":\"unclosed\"}").is_err());
    }

    // ── LLM selection (opt-in) ──

    #[test]
    fn build_selection_prompt_contains_index_and_query() {
        let entries = vec![entry("a", "use pnpm", "p1", 100)];
        let p = build_selection_prompt(&entries, "how to install deps");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].role, ChatRole::System);
        assert!(p[1].content.iter().any(
            |b| matches!(b, ChatBlock::Text { text } if text.contains("how to install deps"))
        ));
        assert!(p[1]
            .content
            .iter()
            .any(|b| matches!(b, ChatBlock::Text { text } if text.contains("a: use pnpm"))));
    }

    #[test]
    fn parse_selection_json_plain_array() {
        assert_eq!(
            parse_selection_json("[\"a\",\"b\"]").unwrap(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn parse_selection_json_tolerates_prose() {
        let text = "Relevant: [\"a\"]\nDone.";
        assert_eq!(parse_selection_json(text).unwrap(), vec!["a"]);
    }

    #[tokio::test]
    async fn llm_select_filters_by_slugs() {
        let (_dir, store) = store();
        store.upsert(entry("a", "pnpm", "p1", 100)).unwrap();
        store.upsert(entry("b", "cargo", "p1", 100)).unwrap();
        let reply = "[\"a\"]".to_string();
        let got = llm_select(&store, "p1", "install deps", move |_prompt| {
            let reply = reply.clone();
            Box::pin(async move { Ok(reply) })
        })
        .await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "a");
    }

    #[tokio::test]
    async fn llm_select_empty_entries_returns_empty() {
        let (_dir, store) = store();
        assert!(llm_select(&store, "p1", "q", |_prompt| {
            Box::pin(async move { Ok("[\"a\"]".to_string()) })
        })
        .await
        .is_empty());
    }

    #[tokio::test]
    async fn llm_select_invalid_reply_returns_empty() {
        let (_dir, store) = store();
        store.upsert(entry("a", "pnpm", "p1", 100)).unwrap();
        assert!(llm_select(&store, "p1", "q", |_prompt| {
            Box::pin(async move { Ok("not json".to_string()) })
        })
        .await
        .is_empty());
    }
}

#[cfg(test)]
mod round10_tests {
    use super::*;

    #[test]
    fn parse_extract_json_tolerates_trailing_commas() {
        let text = "Here you go:\n[{\"name\": \"a\", \"description\": \"d\", \"body\": \"b\",},]";
        let parsed = parse_extract_json(text);
        assert!(parsed.is_ok(), "trailing commas must not fail: {parsed:?}");
    }
}
