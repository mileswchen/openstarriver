//! Resume end-to-end: persist a session through the SQLite service, then
//! resume it in a factory bound to the same persistence and verify the
//! restored log, continued cursor, and fallback semantics.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cordis_rslite::{Context, EventBus};
use starriver_agent_loop::agent_loop::{AgentLoop, AgentLoopConfig};
use starriver_agent_loop::registry::InMemoryAgentRegistry;
use starriver_kernel::agent::{Agent, AgentRegistry, ResumeAgentOptions};
use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::{
    ContentBlock, FinishReason, GenerateOptions, LlmAdapter, LlmError, LlmRuntime, Message,
    MessageId, MessageRole, MessageSource, StreamChunk,
};
use starriver_kernel::persistence::SessionPersistence as _;
use starriver_kernel::session::{SessionEvent, SessionId};
use starriver_session_persistence::SqliteSessionPersistence;

/// Minimal scripted model: yields one text reply per request, mirroring the
/// agent_e2e harness's `ScriptedAdapter` for the resume path.
struct ScriptedAdapter {
    script: Arc<Mutex<VecDeque<String>>>,
}

impl ScriptedAdapter {
    fn new(script: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            script: Arc::new(Mutex::new(script.into())),
        })
    }
}

impl LlmAdapter for ScriptedAdapter {
    fn providers(&self) -> &[String] {
        static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        PROVIDERS.get_or_init(|| vec!["mock".to_string()])
    }
    fn stream(
        &self,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
        let text = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "fallback".into());
        let text_for_block = text.clone();
        Box::pin(futures::stream::iter(vec![
            Ok(StreamChunk::TextDelta { index: 0, text }),
            Ok(StreamChunk::BlockEnd {
                index: 0,
                block: ContentBlock::Text(text_for_block),
            }),
            Ok(StreamChunk::Finish {
                reason: FinishReason::Stop,
                replay_state: None,
            }),
        ]))
    }
}

fn user_message(text: &str) -> Message {
    Message {
        id: MessageId(text.into()),
        role: MessageRole::User,
        content: vec![ContentBlock::Text(text.into())],
        source: MessageSource::User,
    }
}

async fn idle(agent: &Arc<dyn Agent>) {
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
    assert!(result.is_ok(), "driver must converge");
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

fn factory(
    path: std::path::PathBuf,
) -> (Arc<InMemoryAgentRegistry>, Arc<SqliteSessionPersistence>) {
    factory_with_llm(path, None)
}

fn factory_with_llm(
    path: std::path::PathBuf,
    adapter: Option<Arc<dyn LlmAdapter>>,
) -> (Arc<InMemoryAgentRegistry>, Arc<SqliteSessionPersistence>) {
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence_bus = bus.clone();
    let sessions: Arc<dyn starriver_agent_loop::services::SessionStoreView> = Arc::new(
        starriver_session::InMemorySessionStore::new(bus, ctx.clone()),
    );
    let tools: Arc<dyn starriver_agent_loop::services::ToolRuntimeView> = Arc::new(
        starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new()),
    );
    let llm_runtime = starriver_llm::InMemoryLlmRuntime::new(EventBus::new());
    if let Some(adapter) = adapter {
        let _adapter_handle = llm_runtime.register_adapter(adapter).unwrap();
    }
    let llm: Arc<dyn starriver_agent_loop::services::LlmRuntimeView> = Arc::new(llm_runtime);
    let registry = Arc::new(InMemoryAgentRegistry::new());
    let persistence =
        Arc::new(SqliteSessionPersistence::open_with_bus(path, persistence_bus).unwrap());
    let loop_service = AgentLoop::new(
        ctx,
        AgentLoopConfig::default(),
        sessions,
        tools,
        llm,
        Arc::clone(&registry) as Arc<dyn starriver_kernel::agent::AgentRegistry>,
        Some(Arc::clone(&persistence) as Arc<dyn starriver_agent_loop::services::PersistenceView>),
        std::collections::HashMap::new(),
        Arc::new(starriver_kernel::agent::AgentScopeRegistry::new()),
        Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
    );
    let _factory_handle = registry.set_factory(Arc::new(loop_service)).unwrap();
    (registry, persistence)
}

#[tokio::test]
async fn resume_loads_balanced_log_with_repair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let persistence = SqliteSessionPersistence::open(path.clone()).unwrap();
    let id = SessionId("s1".into());
    persistence
        .append(
            id.clone(),
            vec![event(0, "turn/start"), event(1, "user/message")],
            None,
        )
        .await
        .unwrap();
    let loaded = persistence.load(id.clone(), None).await.unwrap();
    assert_eq!(loaded.events[0].event_type, "turn/start");
    assert_eq!(loaded.events[1].event_type, "user/message");
    // A second service on the same file sees the same durable log.
    let second = SqliteSessionPersistence::open(path).unwrap();
    let again = second.load(id.clone(), None).await.unwrap();
    assert_eq!(again.events.len(), loaded.events.len());
}

#[tokio::test]
async fn resume_missing_session_falls_back_to_create() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, _persistence) = factory(dir.path().join("s.db"));
    let handle = registry
        .resume(ResumeAgentOptions {
            resume_session_id: SessionId("missing".into()),
            cwd: None,
            agent_options: starriver_kernel::agent::AgentOptions::default(),
        })
        .await
        .unwrap();
    // NotFound fell back to create: the agent exists and has a session.
    assert_eq!(handle.agent.id().0, "missing");
}

#[tokio::test]
async fn resume_corrupted_session_stays_loud() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let id = SessionId("s1".into());
    // Seed a durable session, then corrupt its committed event row.
    {
        let persistence = SqliteSessionPersistence::open(path.clone()).unwrap();
        persistence
            .append(
                id.clone(),
                vec![event(0, "turn/start"), event(1, "user/message")],
                None,
            )
            .await
            .unwrap();
    }
    // Corrupt a committed prefix row (NOT the tail) so it must stay loud.
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("UPDATE events SET data = '{broken' WHERE seq = 0", [])
        .unwrap();
    drop(db);
    let (registry, _persistence) = factory(path);
    let result = registry
        .resume(ResumeAgentOptions {
            resume_session_id: id,
            cwd: None,
            agent_options: starriver_kernel::agent::AgentOptions::default(),
        })
        .await;
    assert!(result.is_err(), "corruption must stay loud, not fall back");
}

#[tokio::test]
async fn resume_continues_loop_from_balanced_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let id = SessionId("s1".into());
    // First process: create the agent, run one turn, persist, and dispose.
    {
        let adapter = ScriptedAdapter::new(vec!["first reply".into(), "second reply".into()]);
        let (registry, persistence) = factory_with_llm(path.clone(), Some(adapter));
        let handle = registry
            .create(starriver_kernel::agent::CreateAgentOptions {
                session_id: id.clone(),
                seed: Vec::new(),
                cwd: Some("/tmp".into()),
                agent_options: starriver_kernel::agent::AgentOptions::default(),
    agent_preset: None,
        })
            .await
            .unwrap();
        handle.agent.followup(user_message("hi"));
        idle(&handle.agent).await;
        persistence.flush().await.unwrap();
        handle.dispose();
    }
    // Second process: resume the same session, run a new turn, and verify the
    // durable log continues with contiguous seq (no duplicate seed).
    {
        let adapter = ScriptedAdapter::new(vec!["third reply".into()]);
        let (registry, persistence) = factory_with_llm(path.clone(), Some(adapter));
        let handle = registry
            .resume(ResumeAgentOptions {
                resume_session_id: id.clone(),
                cwd: None,
                agent_options: starriver_kernel::agent::AgentOptions::default(),
            })
            .await
            .unwrap();
        handle.agent.followup(user_message("again"));
        idle(&handle.agent).await;
        persistence.flush().await.unwrap();
        let inspection = persistence.load(id.clone(), None).await.unwrap();
        let seqs = inspection.events.iter().map(|e| e.seq).collect::<Vec<_>>();
        assert_eq!(
            seqs,
            (0..seqs.len() as u64).collect::<Vec<_>>(),
            "resumed log must remain seq-contiguous"
        );
        // The resumed turn is a new turn/start after the balanced close.
        let types = inspection
            .events
            .iter()
            .map(|e| e.event_type.as_str())
            .collect::<Vec<_>>();
        let turn_starts = types.iter().filter(|t| **t == "turn/start").count();
        assert_eq!(turn_starts, 2, "create turn + resumed turn");
        // end-seed appears exactly once: the durable lineage boundary.
        let end_seeds = types.iter().filter(|t| **t == "session/end-seed").count();
        assert_eq!(end_seeds, 1);
        handle.dispose();
    }
}

#[tokio::test]
async fn resume_does_not_replicate_end_seed_across_repeated_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let id = SessionId("s1".into());
    // Seed a balanced durable log directly through the persistence service.
    {
        let persistence = SqliteSessionPersistence::open(path.clone()).unwrap();
        persistence
            .append(
                id.clone(),
                vec![event(0, "turn/start"), event(1, "turn/end")],
                None,
            )
            .await
            .unwrap();
    }
    for _ in 0..3 {
        let (registry, persistence) =
            factory_with_llm(path.clone(), Some(ScriptedAdapter::new(vec!["x".into()])));
        let handle = registry
            .resume(ResumeAgentOptions {
                resume_session_id: id.clone(),
                cwd: None,
                agent_options: starriver_kernel::agent::AgentOptions::default(),
            })
            .await
            .unwrap();
        persistence.flush().await.unwrap();
        handle.dispose();
    }
    let persistence = SqliteSessionPersistence::open(path).unwrap();
    let inspection = persistence.load(id.clone(), None).await.unwrap();
    let end_seeds = inspection
        .events
        .iter()
        .filter(|e| e.event_type == "session/end-seed")
        .count();
    assert_eq!(end_seeds, 1, "end-seed must persist exactly once");
}

#[tokio::test]
async fn two_sessions_persist_independently() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let (registry, persistence) = factory_with_llm(
        path.clone(),
        Some(ScriptedAdapter::new(vec!["a".into(), "b".into()])),
    );
    let a = SessionId("a".into());
    let b = SessionId("b".into());
    let handle_a = registry
        .create(starriver_kernel::agent::CreateAgentOptions {
            session_id: a.clone(),
            seed: Vec::new(),
            cwd: Some("/tmp".into()),
            agent_options: starriver_kernel::agent::AgentOptions::default(),
    agent_preset: None,
        })
        .await
        .unwrap();
    let handle_b = registry
        .create(starriver_kernel::agent::CreateAgentOptions {
            session_id: b.clone(),
            seed: Vec::new(),
            cwd: Some("/tmp".into()),
            agent_options: starriver_kernel::agent::AgentOptions::default(),
    agent_preset: None,
        })
        .await
        .unwrap();
    handle_a.agent.followup(user_message("for a"));
    handle_b.agent.followup(user_message("for b"));
    idle(&handle_a.agent).await;
    idle(&handle_b.agent).await;
    persistence.flush().await.unwrap();
    let loaded_a = persistence.load(a, None).await.unwrap();
    let loaded_b = persistence.load(b, None).await.unwrap();
    assert_eq!(loaded_a.events.len(), loaded_b.events.len());
    assert_eq!(
        loaded_a
            .events
            .iter()
            .filter(|e| e.event_type == "turn/start")
            .count(),
        1
    );
    assert_eq!(
        loaded_b
            .events
            .iter()
            .filter(|e| e.event_type == "turn/start")
            .count(),
        1
    );
}
