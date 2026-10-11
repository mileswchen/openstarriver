// Cross-agent history ingestion (docs/37).
//
// A `HistorySource` knows how to discover and parse one agent's local session
// history into a `NormalizedTranscript`. The `SourceRegistry` runs them all
// behind an incremental watermark, feeding new transcripts to the extraction
// pipeline which persists memories tagged with the originating agent.
//
// Red line: only user/assistant plain text ever reaches extraction (the
// `ChatMessage` conversion in each source already drops tool results, thinking,
// and everything else).

mod claude_code;
mod codex;
mod declarative;
mod dsh;
mod pipeline;
mod state;
mod zcode;

pub use claude_code::ClaudeCodeSource;
pub use codex::CodexSource;
pub use declarative::{
    declarative_sources_from_yaml, DeclarativeSource, DeclarativeSourceConfig,
    DeclarativeSourcesFile,
};
pub use dsh::DshSource;
pub use pipeline::{extract_draft_entries, ExtractLlm, Extractor};
pub use state::IngestState;
pub use zcode::ZcodeSource;

use std::path::{Path, PathBuf};

use crate::chat::ChatMessage;
use crate::store::{MemoryStore, GLOBAL_PROJECT};

/// One session file found by `discover`, with the cheapest possible metadata
/// (no content parsing yet — that happens only for files past the watermark).
pub struct DiscoveredSession {
    pub path: PathBuf,
    /// File mtime in unix millis — the watermark key.
    pub mtime_millis: u64,
    /// Working directory the session ran in, when the format records it.
    pub project_cwd: Option<PathBuf>,
    pub session_id: Option<String>,
}

/// Agent-agnostic transcript: everything downstream needs to extract memories.
pub struct NormalizedTranscript {
 /// Which agent's history this came from ("", "",...).
    pub source_agent: String,
    pub session_id: Option<String>,
    pub project_cwd: Option<PathBuf>,
    /// User/assistant plain-text turns only (redaction applied at parse time).
    pub turns: Vec<ChatMessage>,
}

/// A readable local history store of one agent. Implementations are pure
/// parsing — no LLM, no writes.
pub trait HistorySource: Send + Sync {
 /// Stable identifier: a built-in source id, or a user-declared source id
    /// from sources.yaml.
    fn id(&self) -> &str;
    /// Find candidate session files. Must be cheap-ish (metadata only).
    fn discover(&self) -> Vec<DiscoveredSession>;
    /// Parse one discovered session into normalized turns.
    fn read(&self, session: &DiscoveredSession) -> anyhow::Result<NormalizedTranscript>;
}

/// Derive the memory project id from a session working directory. Uses the
/// munged absolute path (leading separator dropped, separators → `-`), which
/// matches how the desktop layer keys per-workspace memory today.
pub fn project_id_from_cwd(cwd: &Path) -> String {
    let owned = cwd.to_string_lossy();
    let s = owned.trim_start_matches('/');
    let slug: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        GLOBAL_PROJECT.to_string()
    } else {
        slug
    }
}

/// The project bucket a transcript belongs to (`_global` when its cwd is
/// unknown).
pub fn transcript_project(t: &NormalizedTranscript) -> String {
    t.project_cwd
        .as_deref()
        .map(project_id_from_cwd)
        .unwrap_or_else(|| GLOBAL_PROJECT.to_string())
}

/// Per-source outcome of one ingestion sweep.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SourceReport {
    pub scanned: usize,
    /// Sessions actually read (new or changed since the watermark).
    pub processed: usize,
    /// Sessions skipped because they produced no extractable text.
    pub skipped: usize,
    /// Memories persisted.
    pub memories: usize,
    /// Read/parse failures (reported, never fatal).
    pub errors: usize,
}

#[derive(Debug, Default)]
pub struct IngestReport {
    pub per_source: Vec<(String, SourceReport)>,
}

impl IngestReport {
    pub fn total_memories(&self) -> usize {
        self.per_source.iter().map(|(_, r)| r.memories).sum()
    }
}

/// The built-in sources for the agents this repo knows about, rooted at the
/// user's home directory.
pub fn builtin_sources(home: &Path) -> Vec<Box<dyn HistorySource>> {
    vec![
        Box::new(CodexSource::new(home)),
        Box::new(ClaudeCodeSource::new(home)),
        Box::new(DshSource::new(home)),
        Box::new(ZcodeSource::new(home)),
    ]
}

/// Built-ins + any user-declared sources from `<memory_root>/sources.yaml`
/// (zero-code onboarding for any new agent).
pub fn configured_sources(home: &Path, memory_root: &Path) -> Vec<Box<dyn HistorySource>> {
    let mut sources = builtin_sources(home);
    let yaml_path = memory_root.join("sources.yaml");
    if let Ok(yaml) = std::fs::read_to_string(&yaml_path) {
        for s in declarative_sources_from_yaml(&yaml) {
            sources.push(Box::new(s));
        }
    }
    sources
}

/// One-time move of the pre-extraction root (`~/.starriver/starcode/memory`)
/// to the shared cross-agent root (`~/.starriver/memory`). Failure is silent —
/// both roots remain readable, worst case we re-extract.
pub fn migrate_legacy_root(home: &Path, new_root: &Path) {
    let legacy = home.join(".starriver").join("starcode").join("memory");
    if legacy.is_dir() && !new_root.exists() {
        if let Some(parent) = new_root.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::rename(&legacy, new_root);
    }
}

pub struct SourceRegistry {
    sources: Vec<Box<dyn HistorySource>>,
}

impl Default for SourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
        }
    }

    pub fn with_builtins(home: &Path) -> Self {
        let mut reg = Self::new();
        for s in builtin_sources(home) {
            reg.register(s);
        }
        reg
    }

    /// Built-ins plus declarative sources from `<memory_root>/sources.yaml`.
    pub fn configured(home: &Path, memory_root: &Path) -> Self {
        let mut reg = Self::new();
        for s in configured_sources(home, memory_root) {
            reg.register(s);
        }
        reg
    }

    pub fn register(&mut self, source: Box<dyn HistorySource>) {
        self.sources.push(source);
    }

    pub fn ids(&self) -> Vec<&str> {
        self.sources.iter().map(|s| s.id()).collect()
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Run one incremental sweep: for every source, read sessions past the
    /// watermark and extract memories via `extractor` (when an LLM is wired;
    /// otherwise transcripts are only counted). Failures are per-session and
    /// never abort the sweep; the watermark only advances on success.
    pub async fn ingest_all(
        &self,
        _store: &MemoryStore,
        extractor: Option<&Extractor<'_>>,
        state: &mut IngestState,
        now: i64,
    ) -> IngestReport {
        let mut report = IngestReport::default();
        for source in &self.sources {
            let mut sr = SourceReport::default();
            for session in source.discover() {
                sr.scanned += 1;
                if !state.is_new(source.id(), &session.path, session.mtime_millis) {
                    continue;
                }
                let transcript = match source.read(&session) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(source = source.id(), path = %session.path.display(), error = %e, "history read failed");
                        sr.errors += 1;
                        continue;
                    }
                };
                let text = crate::extract::extractable_text(&transcript.turns);
                if text.trim().is_empty() {
                    sr.skipped += 1;
                    state.mark(source.id(), &session.path, session.mtime_millis);
                    continue;
                }
                let Some(extractor) = extractor else {
                    // No extractor wired (no model configured): count only —
                    // advancing the watermark here would permanently swallow
                    // all history before the first model is configured.
                    sr.skipped += 1;
                    continue;
                };
                let produced = match extractor.extract(&transcript, &text, now).await {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!(source = source.id(), error = %e, "memory extraction failed");
                        sr.errors += 1;
                        // BUG-70: the watermark only advances on success —
                        // a transient LLM failure must leave the session
                        // eligible for the next sweep.
                        continue;
                    }
                };
                sr.memories += produced;
                sr.processed += 1;
                // Re-stat after the read: a live session appended within the
                // same millisecond as the discovery mtime would otherwise be
                // marked "seen" past its final content forever.
                let final_mtime = mtime_millis(&session.path).max(session.mtime_millis);
                state.mark(source.id(), &session.path, final_mtime);
            }
            report.per_source.push((source.id().to_string(), sr));
        }
        report
    }
}

/// Walk `root` recursively collecting files matching `extension`. Missing
/// roots yield an empty list (an agent simply isn't installed).
pub(crate) fn walk_jsonl(root: &Path, extension: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl(&path, extension, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some(extension) {
            out.push(path);
        }
    }
}

pub(crate) fn read_json_lines(path: &Path) -> anyhow::Result<Vec<String>> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    Ok(std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .collect())
}

pub(crate) fn mtime_millis(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Follow dot-paths (`payload.cwd`) into a JSON value, returning the first hit.
pub(crate) fn dig<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = value;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// Collect text from a content field that may be a plain string or an array of
/// `{type, text}` blocks; only `text`-ish blocks are kept (thinking/tool output
/// dropped — the redaction red line).
pub(crate) fn content_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => {
            let mut out = String::new();
            for item in items {
                let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if ty != "text" && ty != "input_text" && ty != "output_text" {
                    continue;
                }
                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(t);
                }
            }
            out
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatBlock, ChatRole};

    #[test]
    fn project_id_from_cwd_munges_path() {
        assert_eq!(
            project_id_from_cwd(Path::new("/Users/xiaomo-macpro/Documents/ccprj/starriver")),
            "Users-xiaomo-macpro-Documents-ccprj-starriver"
        );
    }

    #[test]
    fn project_id_root_falls_back_to_global() {
        assert_eq!(project_id_from_cwd(Path::new("/")), GLOBAL_PROJECT);
    }

    #[test]
    fn transcript_project_unknown_cwd_is_global() {
        let t = NormalizedTranscript {
            source_agent: "codex".into(),
            session_id: None,
            project_cwd: None,
            turns: vec![],
        };
        assert_eq!(transcript_project(&t), GLOBAL_PROJECT);
    }

    #[test]
    fn content_text_string_and_blocks() {
        assert_eq!(content_text(&serde_json::json!("hello")), "hello");
        let blocks = serde_json::json!([
            {"type": "text", "text": "a"},
            {"type": "thinking", "text": "SECRET"},
            {"type": "tool_result", "text": "output"},
            {"type": "input_text", "text": "b"},
        ]);
        assert_eq!(content_text(&blocks), "a b");
    }

    #[test]
    fn dig_walks_dot_paths() {
        let v = serde_json::json!({"payload": {"cwd": "/tmp/x"}});
        assert_eq!(
            dig(&v, "payload.cwd").and_then(|v| v.as_str()),
            Some("/tmp/x")
        );
        assert!(dig(&v, "payload.missing").is_none());
    }

    #[tokio::test]
    async fn ingest_all_without_llm_counts_only() {
        // A source with one fixture session, no extractor wired: the sweep must
        // mark the watermark but persist nothing.
        let dir = tempfile::tempdir().unwrap();
        let sess = dir.path().join("s1.jsonl");
        std::fs::write(
            &sess,
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        )
        .unwrap();
        struct Fixed(std::path::PathBuf);
        impl HistorySource for Fixed {
            fn id(&self) -> &str {
                "fixed"
            }
            fn discover(&self) -> Vec<DiscoveredSession> {
                vec![DiscoveredSession {
                    path: self.0.join("s1.jsonl"),
                    mtime_millis: 1234,
                    project_cwd: Some(self.0.clone()),
                    session_id: None,
                }]
            }
            fn read(&self, _s: &DiscoveredSession) -> anyhow::Result<NormalizedTranscript> {
                Ok(NormalizedTranscript {
                    source_agent: "fixed".into(),
                    session_id: None,
                    project_cwd: Some(self.0.clone()),
                    turns: vec![ChatMessage {
                        role: ChatRole::User,
                        content: vec![ChatBlock::Text { text: "hi".into() }],
                    }],
                })
            }
        }
        let _ = &sess;
        let mut reg = SourceRegistry::new();
        reg.register(Box::new(Fixed(dir.path().to_path_buf())));
        let store = MemoryStore::new(dir.path().join("m"), crate::store::MemoryConfig::default());
        let mut state = IngestState::in_memory();
        let report = reg.ingest_all(&store, None, &mut state, 1_000).await;
        assert_eq!(report.per_source[0].1.scanned, 1);
        // No extractor wired (no model configured): count-only — the
        // watermark must NOT advance or the first sweep before a model is
        // configured permanently swallows all history.
        assert_eq!(report.per_source[0].1.skipped, 1);
        assert_eq!(report.per_source[0].1.processed, 0);
        assert_eq!(report.per_source[0].1.memories, 0);
        // Second sweep: still eligible (nothing was marked).
        let report2 = reg.ingest_all(&store, None, &mut state, 1_000).await;
        assert_eq!(report2.per_source[0].1.scanned, 1);
        assert_eq!(report2.per_source[0].1.skipped, 1);
    }
}


// ── BUG-70 regression (round 5): the watermark only advances on success ──
#[cfg(test)]
mod round5_tests {
    use super::*;
    use crate::ingest::pipeline::ExtractLlm;
    use futures::future::BoxFuture;
    use std::sync::Arc;

    struct Fixed(std::path::PathBuf);
    impl HistorySource for Fixed {
        fn id(&self) -> &str {
            "fixed"
        }
        fn discover(&self) -> Vec<DiscoveredSession> {
            vec![DiscoveredSession {
                path: self.0.join("s1.jsonl"),
                mtime_millis: 1234,
                project_cwd: Some(self.0.clone()),
                session_id: None,
            }]
        }
        fn read(&self, _s: &DiscoveredSession) -> anyhow::Result<NormalizedTranscript> {
            Ok(NormalizedTranscript {
                source_agent: "fixed".into(),
                session_id: None,
                project_cwd: Some(self.0.clone()),
                turns: vec![crate::chat::ChatMessage {
                    role: crate::chat::ChatRole::User,
                    content: vec![crate::chat::ChatBlock::Text {
                        text: "we use pnpm for everything".into(),
                    }],
                }],
            })
        }
    }

    struct FlakyLlm(atomic_bool);
    type atomic_bool = std::sync::atomic::AtomicBool;
    impl ExtractLlm for FlakyLlm {
        fn chat(&self, _prompt: String) -> BoxFuture<'static, Result<String, String>> {
            let down = self.0.load(std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if down {
                    Err("provider down".into())
                } else {
                    Ok(r#"[{"name":"uses-pnpm","description":"pnpm","body":"deps via pnpm"}]"#
                        .to_string())
                }
            })
        }
    }

    /// The doc contract: "the watermark only advances on success" — one
    /// transient LLM failure must NOT permanently skip the session.
    #[tokio::test]
    async fn extraction_failure_does_not_advance_the_watermark() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("s1.jsonl"), "{}\n").unwrap();
        let mut reg = SourceRegistry::new();
        reg.register(Box::new(Fixed(dir.path().to_path_buf())));
        let store =
            MemoryStore::new(dir.path().join("m"), crate::store::MemoryConfig::default());

        // First sweep with the provider down: the session errors, and the
        // watermark must NOT mark it processed.
        let down = Arc::new(FlakyLlm(std::sync::atomic::AtomicBool::new(true)));
        let extractor = crate::ingest::pipeline::Extractor::new(&store, down);
        let mut state = IngestState::in_memory();
        let report = reg.ingest_all(&store, Some(&extractor), &mut state, 1_000).await;
        assert_eq!(report.per_source[0].1.errors, 1);
        assert_eq!(report.per_source[0].1.processed, 0);

        // Provider recovered: the same session must be retried (and succeed).
        let up = Arc::new(FlakyLlm(std::sync::atomic::AtomicBool::new(false)));
        let extractor = crate::ingest::pipeline::Extractor::new(&store, up);
        let report2 = reg.ingest_all(&store, Some(&extractor), &mut state, 1_000).await;
        assert_eq!(
            report2.per_source[0].1.processed, 1,
            "a transient extraction failure must be retried on the next sweep"
        );
        assert_eq!(report2.per_source[0].1.memories, 1);
    }
}
