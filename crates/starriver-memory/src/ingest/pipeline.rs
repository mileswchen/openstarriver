// Extraction pipeline: transcript → LLM → parsed drafts → persisted memories.

use std::sync::Arc;

use futures::future::BoxFuture;

use crate::chat::ChatMessage;
use crate::cost::{estimate_tokens, CostMode, TokenBudget};
use crate::extract::{
    extract_prompt, is_transient_policy_fact, parse_extract_json,
};
use crate::ingest::{transcript_project, NormalizedTranscript};
use crate::store::{slugify, MemoryEntry, MemorySource, MemoryStore};

/// LLM seam for extraction. The host implements this over its provider
/// directory; the standalone memory-server implements it over direct provider
/// calls. Keeps starriver-memory free of any llm-crate dependency.
pub trait ExtractLlm: Send + Sync {
    /// One-shot chat completion over a plain prompt; returns the raw reply
    /// text (expected to contain a JSON array of drafts).
    fn chat(&self, prompt_text: String) -> BoxFuture<'static, Result<String, String>>;
}

/// Cap on the conversation slice handed to the extraction model (~token bound;
/// keeps a single huge session from costing a fortune). The TAIL is kept —
/// the end of a session carries its conclusions.
pub const MAX_CONVERSATION_CHARS: usize = 60_000;

pub struct Extractor<'a> {
    store: &'a MemoryStore,
    llm: Arc<dyn ExtractLlm>,
    /// Cost meter for the LLM calls below (docs/36 §12.1). `None` (the
    /// `Extractor::new` default) = unmetered — backward compatible for hosts
    /// that have not wired a budget yet.
    budget: Option<(Arc<TokenBudget>, CostMode)>,
}

impl<'a> Extractor<'a> {
    pub fn new(store: &'a MemoryStore, llm: Arc<dyn ExtractLlm>) -> Self {
        Self {
            store,
            llm,
            budget: None,
        }
    }

    /// Attach a cost meter. `CostMode::Auto` (session-exit background
    /// extraction) is denied once the session budget is exhausted — extraction
    /// then silently stops (Ok(0) + warn). `CostMode::Manual` (the
    /// `memory.ingest` sweep, memory-server writes) records usage but is never
    /// denied. Returns `self` for `Extractor::new(..).with_budget(..)` chaining.
    pub fn with_budget(mut self, budget: Arc<TokenBudget>, mode: CostMode) -> Self {
        self.budget = Some((budget, mode));
        self
    }

    /// Pre-call gate: false → this AUTO call must be skipped (budget blown).
    /// Logs the cost-stop once per denial (the host-side event seam polls
    /// `TokenBudget::exceeded()`).
    fn budget_allows(&self, prompt_text: &str) -> bool {
        match &self.budget {
            None => true,
            Some((_budget, CostMode::Manual)) => {
                let _ = prompt_text;
                true
            }
            Some((budget, CostMode::Auto)) => {
                if budget.allows() {
                    true
                } else {
                    let n = budget.note_denied();
                    tracing::warn!(
                        denied = n,
                        used = budget.used(),
                        limit = ?budget.limit(),
                        "memory token budget exhausted — auto extraction stopped"
                    );
                    false
                }
            }
        }
    }

    /// Post-call record: both modes charge estimated prompt + completion
    /// tokens so the panel/status readout covers manual writes too.
    fn budget_record(&self, prompt_text: &str, reply: &str) {
        if let Some((budget, _)) = &self.budget {
            budget.charge(
                estimate_tokens(prompt_text.len()),
                estimate_tokens(reply.len()),
            );
        }
    }

    /// Extract memories from one transcript's already-redacted text and
    /// persist them. Returns how many entries were written. Errors from the
    /// LLM or parsing propagate (the caller decides whether to retry).
    pub async fn extract(
        &self,
        transcript: &NormalizedTranscript,
        redacted_text: &str,
        now: i64,
    ) -> anyhow::Result<usize> {
        let conversation = tail_chars(redacted_text, MAX_CONVERSATION_CHARS);
        let prompt = extract_prompt(&conversation);
        let prompt_text = prompt
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        crate::chat::ChatBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if !self.budget_allows(&prompt_text) {
            return Ok(0);
        }
        let reply = self
            .llm
            .chat(prompt_text.clone())
            .await
            .map_err(anyhow::Error::msg)?;
        self.budget_record(&prompt_text, &reply);
        let drafts = parse_extract_json(&reply).map_err(anyhow::Error::msg)?;
        let project = transcript_project(transcript);
        let entries = extract_draft_entries(&drafts, &project, &transcript.source_agent, now);
        for entry in &entries {
            self.store.upsert_disambiguated(entry.clone())?;
        }
        Ok(entries.len())
    }

    /// Extract from an in-process conversation (the starriver self-source at
    /// loop exit) — same pipeline, source_agent = "starriver".
    pub async fn extract_chat(
        &self,
        messages: &[ChatMessage],
        project: &str,
        source_agent: &str,
        now: i64,
    ) -> anyhow::Result<usize> {
        let text = crate::extract::extractable_text(messages);
        if text.trim().is_empty() {
            return Ok(0);
        }
        let transcript = NormalizedTranscript {
            source_agent: source_agent.to_string(),
            session_id: None,
            project_cwd: None,
            turns: Vec::new(),
        };
        // Reuse extract() but with the caller-specified project: build the
        // entries directly rather than via transcript_project.
        let conversation = tail_chars(&text, MAX_CONVERSATION_CHARS);
        let prompt = extract_prompt(&conversation);
        let prompt_text = prompt
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        crate::chat::ChatBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if !self.budget_allows(&prompt_text) {
            return Ok(0);
        }
        let reply = self
            .llm
            .chat(prompt_text.clone())
            .await
            .map_err(anyhow::Error::msg)?;
        self.budget_record(&prompt_text, &reply);
        let drafts = parse_extract_json(&reply).map_err(anyhow::Error::msg)?;
        let _ = &transcript;
        let entries = extract_draft_entries(&drafts, project, source_agent, now);
        for entry in &entries {
            self.store.upsert_disambiguated(entry.clone())?;
        }
        Ok(entries.len())
    }
}

/// Turn parsed drafts into persisted entries (timestamps/project/source filled
/// in here), dropping transient sandbox/permission claims — the extraction
/// red line (`is_transient_policy_fact`): a read-only session must not bake
/// "writes are refused here" into a durable memory. Exposed for hosts that
/// already have drafts in hand.
pub fn extract_draft_entries(
    drafts: &[crate::extract::MemoryEntryDraft],
    project: &str,
    source_agent: &str,
    now: i64,
) -> Vec<MemoryEntry> {
    drafts
        .iter()
        .filter(|d| !d.name.trim().is_empty() && !d.body.trim().is_empty())
        .filter(|d| !is_transient_policy_fact(&d.name, &d.description, &d.body))
        .map(|d| MemoryEntry {
            name: slugify(&d.name),
            description: d.description.trim().to_string(),
            body: d.body.trim().to_string(),
            created: now,
            updated: now,
            source: MemorySource::Extract,
            source_agent: source_agent.to_string(),
            project: project.to_string(),
        })
        .collect()
}

fn tail_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatBlock, ChatRole};
    use crate::extract::MemoryEntryDraft;
    use crate::store::{MemoryConfig, DEFAULT_SOURCE_AGENT};

    struct FixedLlm(Result<String, String>);

    impl ExtractLlm for FixedLlm {
        fn chat(&self, _prompt: String) -> BoxFuture<'static, Result<String, String>> {
            let reply = self.0.clone();
            Box::pin(async move { reply })
        }
    }

    /// LLM that counts its calls and always returns one draft — the cost-gate
    /// tests assert on how many calls actually went through.
    #[derive(Default)]
    struct CountingLlm {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ExtractLlm for CountingLlm {
        fn chat(&self, _prompt: String) -> BoxFuture<'static, Result<String, String>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                Ok(
                    r#"[{"name":"uses-pnpm","description":"use pnpm","body":"deps via pnpm"}]"#
                        .to_string(),
                )
            })
        }
    }

    fn store() -> (tempfile::TempDir, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memory");
        let store = MemoryStore::new(root, MemoryConfig::default());
        (dir, store)
    }

    #[test]
    fn draft_entries_fill_metadata() {
        let drafts = vec![MemoryEntryDraft {
            name: "Use pnpm!".into(),
            description: "deps via pnpm".into(),
            body: "This project uses pnpm.".into(),
        }];
        let entries = extract_draft_entries(&drafts, "proj", "codex", 42);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "use-pnpm");
        assert_eq!(entries[0].source_agent, "codex");
        assert_eq!(entries[0].project, "proj");
        assert_eq!(entries[0].source, MemorySource::Extract);
    }

    #[test]
    fn draft_entries_drop_empty() {
        let drafts = vec![
            MemoryEntryDraft {
                name: "  ".into(),
                description: "d".into(),
                body: "".into(),
            },
            MemoryEntryDraft {
                name: "n".into(),
                description: "d".into(),
                body: "".into(),
            },
        ];
        assert!(extract_draft_entries(&drafts, "p", "codex", 1).is_empty());
    }

    /// The game004 regression (2026-09-12): a plan-mode session's write
    /// refusals were extracted as a durable "read-only sandbox, write via API"
    /// fact. The extract funnel must drop such drafts and keep real ones.
    #[test]
    fn draft_entries_drop_transient_sandbox_facts() {
        let drafts = vec![
            MemoryEntryDraft {
                name: "sandbox-readonly-write-via-codeup-api".into(),
                description: "Sandbox is read-only; file writes must go through Codeup.".into(),
                body: "file-writing tools are refused; use the open API instead.".into(),
            },
            MemoryEntryDraft {
                name: "uses-pnpm".into(),
                description: "use pnpm".into(),
                body: "deps via pnpm".into(),
            },
        ];
        let entries = extract_draft_entries(&drafts, "proj", "starriver", 42);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "uses-pnpm");
    }

    #[tokio::test]
    async fn extractor_never_persists_transient_sandbox_facts() {
        let (_d, store) = store();
        let llm = Arc::new(FixedLlm(Ok(
            r#"[{"name":"sandbox-readonly","description":"the sandbox is read-only","body":"writes are refused here, use the codeup api"},
                {"name":"uses-pnpm","description":"use pnpm","body":"deps via pnpm"}]"#
                .to_string(),
        )));
        let ex = Extractor::new(&store, llm);
        let chat = vec![ChatMessage::user("make a game")];
        let n = ex
            .extract_chat(&chat, "p", DEFAULT_SOURCE_AGENT, 1)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let list = store.list("p").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "uses-pnpm");
    }

    #[tokio::test]
    async fn extractor_persists_llm_drafts() {
        let (_d, store) = store();
        let llm = Arc::new(FixedLlm(Ok(
            r#"[{"name":"uses-pnpm","description":"use pnpm","body":"deps via pnpm"}]"#.to_string(),
        )));
        let ex = Extractor::new(&store, llm);
        let transcript = NormalizedTranscript {
            source_agent: "codex".into(),
            session_id: Some("s1".into()),
            project_cwd: Some("/tmp/proj".into()),
            turns: vec![ChatMessage {
                role: ChatRole::User,
                content: vec![ChatBlock::Text {
                    text: "install deps".into(),
                }],
            }],
        };
        let n = ex
            .extract(&transcript, "[user] install deps\n", 100)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let list = store.list("tmp-proj").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].source_agent, "codex");
        assert_eq!(list[0].name, "uses-pnpm");
    }

    #[tokio::test]
    async fn extractor_llm_error_propagates() {
        let (_d, store) = store();
        let llm = Arc::new(FixedLlm(Err("provider down".into())));
        let ex = Extractor::new(&store, llm);
        let transcript = NormalizedTranscript {
            source_agent: DEFAULT_SOURCE_AGENT.into(),
            session_id: None,
            project_cwd: None,
            turns: vec![],
        };
        assert!(ex.extract(&transcript, "[user] hi\n", 1).await.is_err());
    }

    #[test]
    fn tail_chars_keeps_tail_on_char_boundary() {
        assert_eq!(tail_chars("abcdef", 3), "def");
        // Multi-byte chars must not split.
        let s = "中文中文中文";
        let t = tail_chars(s, 6);
        assert!(s.ends_with(&t));
    }

    // ── cost gating (docs/36 §12.1) ──

    fn chat_one_message() -> Vec<ChatMessage> {
        vec![ChatMessage {
            role: ChatRole::User,
            content: vec![ChatBlock::Text {
                text: "install deps with pnpm".into(),
            }],
        }]
    }

    #[tokio::test]
    async fn auto_budget_stops_extraction_once_exhausted() {
        let (_d, store) = store();
        let llm = Arc::new(CountingLlm::default());
        let budget = Arc::new(TokenBudget::with_limit(1)); // one call blows it
        let ex = Extractor::new(&store, Arc::clone(&llm) as Arc<dyn ExtractLlm>)
            .with_budget(Arc::clone(&budget), CostMode::Auto);
        let chat = chat_one_message();

        // First call is allowed and crosses the limit.
        assert_eq!(
            ex.extract_chat(&chat, "p", "starriver", 1).await.unwrap(),
            1
        );
        assert_eq!(llm.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(budget.exceeded().is_some());

        // Second AUTO call is denied before the LLM: 0 memories, no new call.
        assert_eq!(
            ex.extract_chat(&chat, "p", "starriver", 2).await.unwrap(),
            0
        );
        assert_eq!(llm.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(budget.denied_calls(), 1);
    }

    #[tokio::test]
    async fn manual_mode_records_but_never_denies() {
        let (_d, store) = store();
        let llm = Arc::new(CountingLlm::default());
        let budget = Arc::new(TokenBudget::with_limit(1));
        let ex = Extractor::new(&store, Arc::clone(&llm) as Arc<dyn ExtractLlm>)
            .with_budget(Arc::clone(&budget), CostMode::Manual);
        let chat = chat_one_message();

        for _ in 0..3 {
            assert_eq!(
                ex.extract_chat(&chat, "p", "starriver", 1).await.unwrap(),
                1
            );
        }
        assert_eq!(llm.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(budget.calls(), 3);
        assert_eq!(budget.denied_calls(), 0);
        assert!(
            budget.exceeded().is_some(),
            "usage is still recorded for the panel"
        );
    }

    #[tokio::test]
    async fn unmetered_extractor_keeps_old_behavior() {
        let (_d, store) = store();
        let llm = Arc::new(CountingLlm::default());
        let ex = Extractor::new(&store, Arc::clone(&llm) as Arc<dyn ExtractLlm>);
        let chat = chat_one_message();
        assert_eq!(
            ex.extract_chat(&chat, "p", "starriver", 1).await.unwrap(),
            1
        );
        assert_eq!(
            ex.extract_chat(&chat, "p", "starriver", 2).await.unwrap(),
            1
        );
        assert_eq!(llm.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
