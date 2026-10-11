// Host-side memory pipeline glue: converts kernel session messages into the
// memory crate's chat mirror and drives background extraction at loop exit
// (docs/36 path B; docs/37 cross-agent).

use std::sync::Arc;

use futures::future::BoxFuture;
use starriver_kernel::llm::{
    ContentBlock as KernelBlock, GenerateOptions, Message as KernelMessage, MessageId, MessageRole,
    MessageSource,
};
use starriver_memory::chat::{ChatBlock, ChatMessage, ChatRole};
use starriver_memory::ingest::{ExtractLlm, Extractor};
use starriver_memory::{MemoryConfig, MemoryStore, DEFAULT_SOURCE_AGENT, GLOBAL_PROJECT};

use crate::RuntimeInner;
use starriver_kernel::llm::LlmRuntime;

/// Manual trigger for the external-agent sweep (protocol `memory.ingest`).
/// Built-ins + sources.yaml sources, incremental watermark, LLM extraction
/// over the active route when one is configured. Returns the IngestReport as
/// protocol JSON.
pub(crate) fn run_ingestion(host: &crate::HostRuntime) -> serde_json::Value {
    let handle = host.runtime_handle();
    let home = std::env::var("HOME").unwrap_or_default();
    let root = host.memory_store.root().to_path_buf();
    let registry =
        starriver_memory::ingest::SourceRegistry::configured(std::path::Path::new(&home), &root);
    let mut state = starriver_memory::ingest::IngestState::load(&root.join("ingest_state.json"));

    // Resolve an extraction route: the Coding Agent's active model when
    // configured, otherwise the first provider's default. No route →
    // count-only sweep.
    let active = host.resolve_agent_config(crate::CODING_AGENT_NAME).model_id;
    let (provider, model) = {
        let settings = host.settings.read().unwrap();
        let entry = settings
            .llm
            .providers
            .iter()
            .find(|p| active.as_ref().map_or(false, |a| &p.id == a))
            .or_else(|| settings.llm.providers.first())
            .cloned();
        match entry {
            Some(e) => (e.id, e.default_model),
            None => (String::new(), String::new()),
        }
    };

    let store = Arc::clone(&host.memory_store);
    let inner = host.inner.clone();
    let llm_routed = !provider.is_empty() && !model.is_empty();
    let report = handle.block_on(async {
        // Short lock: clone the LLM handle out, then run the (potentially
        // long, network-bound) sweep WITHOUT the runtime mutex — trace and
        // timeline reads must stay responsive during ingestion.
        let llm = if llm_routed {
            let guard = inner.lock().await;
            guard.as_ref().map(|runtime| runtime.agent.llm())
        } else {
            None
        };
        let extractor = llm.map(|llm| {
            let host_llm = HostExtractLlm::new(llm, provider.clone(), model.clone(), "ingest");
            Extractor::new(store.as_ref(), Arc::new(host_llm)).with_budget(
                Arc::clone(&host.memory_budget),
                starriver_memory::CostMode::Manual,
            )
        });
        let now = starriver_memory::now_unix();
        registry
            .ingest_all(store.as_ref(), extractor.as_ref(), &mut state, now)
            .await
    });
    state.save();

    serde_json::json!({
        "llmRouted": llm_routed,
        "sources": report
            .per_source
            .iter()
            .map(|(id, r)| serde_json::json!({
                "id": id,
                "scanned": r.scanned,
                "processed": r.processed,
                "skipped": r.skipped,
                "memories": r.memories,
                "errors": r.errors,
            }))
            .collect::<Vec<_>>(),
        "totalMemories": report.total_memories(),
    })
}

/// Protocol `memory.status`: per-project / per-agent inventory of the shared
/// store plus the configured ingestion sources.
pub(crate) fn status_json(host: &crate::HostRuntime) -> serde_json::Value {
    use std::collections::BTreeMap;
    let root = host.memory_store.root().to_path_buf();
    let mut projects: Vec<serde_json::Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_dir() {
                continue;
            }
            let project = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let memories = host.memory_store.list(&project).unwrap_or_default();
            let mut by_agent: BTreeMap<String, usize> = BTreeMap::new();
            for m in &memories {
                *by_agent.entry(m.source_agent.clone()).or_default() += 1;
            }
            projects.push(serde_json::json!({
                "project": project,
                "count": memories.len(),
                "bySourceAgent": by_agent,
            }));
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let registry =
        starriver_memory::ingest::SourceRegistry::configured(std::path::Path::new(&home), &root);
    serde_json::json!({
        "root": root,
        "projects": projects,
        "sources": registry.ids(),
        "watermark": root.join("ingest_state.json").exists(),
        "budget": host.memory_budget.usage_json(),
    })
}

/// Protocol `memory.export`: write the marked section into AGENTS.md /
/// CLAUDE.md under `path` (the project directory).
pub(crate) fn export_for_path(
    host: &crate::HostRuntime,
    path: &str,
) -> anyhow::Result<serde_json::Value> {
    let dir = std::path::PathBuf::from(path);
    if !dir.is_dir() {
        return Err(anyhow::anyhow!("not a directory: {path}"));
    }
    let project = starriver_memory::ingest::project_id_from_cwd(&dir);
    let written =
        starriver_memory::export::export_to_instruction_files(&host.memory_store, &project, &dir)?;
    Ok(serde_json::json!({ "project": project, "written": written }))
}

/// Project a kernel conversation into the memory crate's redacted mirror:
/// user/assistant text survives, reasoning/tool traffic is dropped at the
/// boundary (the extraction red line).
pub(crate) fn kernel_to_chat(messages: &[KernelMessage]) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    for m in messages {
        let role = match m.role {
            MessageRole::User => ChatRole::User,
            MessageRole::Assistant => ChatRole::Assistant,
            MessageRole::System => continue,
        };
        let content: Vec<ChatBlock> = m
            .content
            .iter()
            .filter_map(|b| match b {
                KernelBlock::Text(t) => Some(ChatBlock::Text { text: t.clone() }),
                _ => None,
            })
            .collect();
        if content.is_empty() {
            continue;
        }
        out.push(ChatMessage { role, content });
    }
    out
}

/// `ExtractLlm` over the host's kernel LLM directory (same GenerateOptions
/// shape the memory-selection pass uses).
pub(crate) struct HostExtractLlm {
    llm: Arc<starriver_llm::InMemoryLlmRuntime>,
    provider: String,
    model: String,
    purpose: &'static str,
}

impl HostExtractLlm {
    pub fn new(
        llm: Arc<starriver_llm::InMemoryLlmRuntime>,
        provider: String,
        model: String,
        purpose: &'static str,
    ) -> Self {
        Self {
            llm,
            provider,
            model,
            purpose,
        }
    }
}

impl ExtractLlm for HostExtractLlm {
    fn chat(&self, prompt_text: String) -> BoxFuture<'static, Result<String, String>> {
        let llm = Arc::clone(&self.llm);
        let provider = self.provider.clone();
        let model = self.model.clone();
        let purpose = self.purpose;
        Box::pin(async move {
            let options = GenerateOptions {
                provider,
                model,
                reasoning_effort: None,
                tools: None,
                messages: vec![KernelMessage {
                    id: MessageId(format!("memory-{purpose}").into()),
                    role: MessageRole::User,
                    content: vec![KernelBlock::Text(prompt_text)],
                    source: MessageSource::User,
                }],
                system: None,
                temperature: None,
                max_tokens: Some(2048),
                stop: None,
                session_id: None,
                agent_id: None,
                tool_choice: None,
                cancellation: None,
                purpose: None,
                loop_assembled: false,
            };
            llm.chat(options).await.map_err(|e| e.to_string())
        })
    }
}

/// Collect the redaction-mirrored conversation + LLM handle under a SHORT
/// lock (local sqlite read only). `None` when the runtime is gone or the load
/// fails — callers treat that as "nothing to extract".
pub(crate) async fn collect_session_conversation(
    inner: &Arc<tokio::sync::Mutex<Option<RuntimeInner>>>,
    session_id: &str,
) -> Option<(Vec<KernelMessage>, Arc<starriver_llm::InMemoryLlmRuntime>)> {
    let guard = inner.lock().await;
    let runtime = guard.as_ref()?;
    let messages = match crate::sessions::load_surface_messages(runtime, session_id).await {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(session = session_id, error = %e, "memory extraction: surface load failed");
            return None;
        }
    };
    let llm = runtime.agent.llm();
    let conversation: Vec<KernelMessage> = messages.into_iter().map(|(_, m)| m).collect();
    Some((conversation, llm))
}

/// Background extraction for one finished starriver session (docs/36 path B).
/// Failure-silent: every error path logs at debug and exits — memory is a
/// bonus, never a turn dependency.
pub(crate) fn spawn_session_extraction(
    inner: Arc<tokio::sync::Mutex<Option<RuntimeInner>>>,
    handle: tokio::runtime::Handle,
    session_id: String,
    project_id: Option<String>,
    store: Arc<MemoryStore>,
    budget: Arc<starriver_memory::TokenBudget>,
    cfg: MemoryConfig,
    provider: String,
    model: String,
) {
    if !cfg.enabled {
        return;
    }
    if provider.is_empty() || model.is_empty() {
        tracing::debug!(session = %session_id, "memory extraction: no active route");
        return;
    }
    handle.spawn(async move {
        let result: anyhow::Result<usize> = async {
            // Phase 1 (under the runtime lock, local-fast): collect the
            // conversation + LLM handle, then DROP the lock. Trace/timeline
            // reads take this same mutex — holding it across the extraction
            // LLM call froze the trajectory view for the whole call.
            let Some((conversation, llm)) = collect_session_conversation(&inner, &session_id).await
            else { return Ok(0) };
            let chat = kernel_to_chat(&conversation);
            if chat.is_empty() {
                return Ok(0);
            }
            // Phase 2 (lock-free): extraction with a hard timeout — a hung
            // provider must never pin anything.
            let extractor_llm = HostExtractLlm::new(llm, provider, model, "extract");
            let extractor = Extractor::new(&store, Arc::new(extractor_llm))
                .with_budget(budget, starriver_memory::CostMode::Auto);
            let project = project_id.unwrap_or_else(|| GLOBAL_PROJECT.to_string());
            let now = starriver_memory::now_unix();
            tokio::time::timeout(
                std::time::Duration::from_secs(180),
                extractor.extract_chat(&chat, &project, DEFAULT_SOURCE_AGENT, now),
            )
            .await
            .map_err(|_| anyhow::anyhow!("memory extraction timed out"))?
        }
        .await;
        match result {
            Ok(n) if n > 0 => tracing::info!(session = %session_id, memories = n, "background memory extraction done"),
            Ok(_) => {}
            Err(e) => tracing::debug!(session = %session_id, error = %e, "memory extraction skipped"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel_msg(role: MessageRole, blocks: Vec<KernelBlock>) -> KernelMessage {
        KernelMessage {
            id: MessageId("m".into()),
            role,
            content: blocks,
            source: MessageSource::User,
        }
    }

    /// Regression: spawn_session_extraction used to hold the runtime mutex
    /// across the whole extraction LLM await, freezing session.trace /
    /// timeline (they take the same lock) until the provider replied. The
    /// invariant: once collect_session_conversation returns, a pending
    /// extraction must leave the lock free.
    #[test]
    fn extraction_llm_phase_never_holds_the_runtime_lock() {
        // HostRuntime::new boots its own tokio runtime, so this must be a
        // sync test; the async phases below run on the host's handle.
        let host = std::sync::Arc::new(crate::HostRuntime::new(None).expect("host boots"));
        host.runtime_handle().block_on(async {
            // Phase 1 completes and drops the lock (unknown session → nothing
            // to extract, by design).
            let _ = collect_session_conversation(&host.inner, "no-such-session").await;

            // Simulate phase 2: an extraction future parked on a never-firing LLM.
            struct HangingLlm;
            impl ExtractLlm for HangingLlm {
                fn chat(
                    &self,
                    _prompt: String,
                ) -> futures::future::BoxFuture<'static, Result<String, String>> {
                    Box::pin(std::future::pending())
                }
            }
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(MemoryStore::new(
                dir.path().join("m"),
                MemoryConfig::default(),
            ));
            let extractor = Extractor::new(store.as_ref(), Arc::new(HangingLlm));
            let chat = vec![starriver_memory::chat::ChatMessage::user("remember this")];
            let mut extraction =
                std::pin::pin!(extractor.extract_chat(&chat, "p", DEFAULT_SOURCE_AGENT, 1));
            // Poll once so the LLM future is actually parked inside extraction.
            futures::future::poll_fn::<(), _>(|cx| {
                use std::future::Future;
                use std::task::Poll;
                match extraction.as_mut().poll(cx) {
                    Poll::Ready(r) => panic!("extraction unexpectedly finished: {r:?}"),
                    Poll::Pending => std::task::Poll::Ready(()),
                }
            })
            .await;

            // The runtime lock must be free while extraction is pending —
            // this is exactly what session.trace needs.
            assert!(
                host.inner.try_lock().is_ok(),
                "runtime mutex must not be held while the extraction LLM call is pending"
            );
        });
    }

    #[test]
    fn kernel_to_chat_applies_redaction() {
        let msgs = vec![
            kernel_msg(MessageRole::System, vec![KernelBlock::Text("sys".into())]),
            kernel_msg(
                MessageRole::User,
                vec![KernelBlock::Text("run tests".into())],
            ),
            kernel_msg(
                MessageRole::Assistant,
                vec![
                    KernelBlock::Text("done".into()),
                    KernelBlock::Reasoning("internal".into()),
                ],
            ),
            kernel_msg(
                MessageRole::User,
                vec![KernelBlock::ToolResult {
                    call_id: starriver_kernel::llm::CallId("t1".into()),
                    content: vec![KernelBlock::Text("SECRET".into())],
                    is_error: false,
                }],
            ),
        ];
        let chat = kernel_to_chat(&msgs);
        assert_eq!(chat.len(), 2);
        let text = starriver_memory::extractable_text(&chat);
        assert!(text.contains("run tests"));
        assert!(text.contains("done"));
        assert!(!text.contains("SECRET"));
        assert!(!text.contains("internal"));
    }
}
