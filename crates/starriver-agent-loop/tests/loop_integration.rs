//! End-to-end loop integration through the public AgentLoop + registry API,
//! covering the review blocker fixes (B1/B2/B3).

use starriver_kernel::json::JsonValue;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use cordis_rslite::{Context, EventBus};
use futures::StreamExt;
use starriver_agent_loop::agent_loop::{AgentLoop, AgentLoopConfig};
use starriver_agent_loop::registry::InMemoryAgentRegistry;
use starriver_agent_loop::services::{LlmRuntimeView, SessionStoreView, ToolRuntimeView};
use starriver_kernel::agent::{AgentErrorEvent, AgentOptions, AgentRegistry, CreateAgentOptions};
use starriver_kernel::llm::{
    CallId, FinishReason, GenerateOptions, LlmAdapter, LlmCallConfig, LlmError, LlmRuntime,
    PreparedLlmCall, StreamChunk, ToolSchema,
};
use starriver_kernel::session::SessionId;
use starriver_kernel::tools::{
    ToolError, ToolExecutionInput, ToolExecutionMode, ToolExecutionResult, ToolRunContext,
    ToolRuntime,
};

fn message(text: &str) -> starriver_kernel::llm::Message {
    starriver_kernel::llm::Message {
        id: starriver_kernel::llm::MessageId(text.into()),
        role: starriver_kernel::llm::MessageRole::User,
        content: vec![starriver_kernel::llm::ContentBlock::Text(text.into())],
        source: starriver_kernel::llm::MessageSource::User,
    }
}

/// One adapter whose first stream holds until cancelled and later streams
/// stop immediately (B1: cancel + followup must start a fresh turn).
struct CancelThenStopAdapter {
    providers: Vec<String>,
    first: AtomicBool,
}

/// One adapter that always stops with empty text.
struct StopAdapter {
    providers: Vec<String>,
}

impl LlmAdapter for StopAdapter {
    fn providers(&self) -> &[String] {
        &self.providers
    }
    fn stream(
        &self,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
        use futures::stream;
        Box::pin(stream::iter(vec![Ok(StreamChunk::Finish {
            reason: FinishReason::Stop,
            replay_state: None,
        })]))
    }
}

/// One adapter whose first stream hits max-tokens after a short delay and
/// later streams stop with text (W2 sticky).
struct MaxTokensThenStopAdapter {
    providers: Vec<String>,
    calls: AtomicUsize,
}

impl LlmAdapter for MaxTokensThenStopAdapter {
    fn providers(&self) -> &[String] {
        &self.providers
    }
    fn stream(
        &self,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
        use futures::stream;
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Box::pin(stream::once(async {
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                Ok(StreamChunk::Finish {
                    reason: FinishReason::MaxTokens,
                    replay_state: None,
                })
            }))
        } else {
            Box::pin(stream::iter(vec![
                Ok(StreamChunk::TextDelta {
                    index: 0,
                    text: "after".into(),
                }),
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: starriver_kernel::llm::ContentBlock::Text("after".into()),
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                }),
            ]))
        }
    }
}

/// One adapter that asks for an echo tool call first, then stops (W4 chain).
struct EchoToolThenStopAdapter {
    providers: Vec<String>,
    calls: AtomicUsize,
}

impl LlmAdapter for EchoToolThenStopAdapter {
    fn providers(&self) -> &[String] {
        &self.providers
    }
    fn stream(
        &self,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
        use futures::stream;
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Box::pin(stream::iter(vec![
                Ok(StreamChunk::ToolCallDelta {
                    index: 0,
                    id: CallId("c1".into()),
                    name: Some("echo".into()),
                    arguments_delta: "{}".into(),
                }),
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: starriver_kernel::llm::ContentBlock::ToolCall {
                        id: CallId("c1".into()),
                        name: Some("echo".into()),
                        arguments: JsonValue::Object(Vec::new()),
                    },
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::ToolCalls,
                    replay_state: None,
                }),
            ]))
        } else {
            Box::pin(stream::iter(vec![
                Ok(StreamChunk::TextDelta {
                    index: 0,
                    text: "done".into(),
                }),
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: starriver_kernel::llm::ContentBlock::Text("done".into()),
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                }),
            ]))
        }
    }
}

impl LlmAdapter for CancelThenStopAdapter {
    fn providers(&self) -> &[String] {
        &self.providers
    }
    fn stream(
        &self,
        options: GenerateOptions,
    ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
        use futures::stream;
        if self.first.swap(false, Ordering::SeqCst) {
            let cancellation = options.cancellation.clone();
            Box::pin(
                stream::iter(vec![Ok(StreamChunk::TextDelta {
                    index: 0,
                    text: "partial".into(),
                })])
                .chain(stream::once(async move {
                    if let Some(token) = cancellation {
                        token.cancelled().await;
                    }
                    Err(LlmError::Cancelled)
                })),
            )
        } else {
            Box::pin(stream::iter(vec![
                Ok(StreamChunk::TextDelta {
                    index: 0,
                    text: "done".into(),
                }),
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: starriver_kernel::llm::ContentBlock::Text("done".into()),
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                }),
            ]))
        }
    }
}

/// One adapter that always asks for a tool call (B3).
struct ToolCallAdapter {
    providers: Vec<String>,
}

impl LlmAdapter for ToolCallAdapter {
    fn providers(&self) -> &[String] {
        &self.providers
    }
    fn stream(
        &self,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
        use futures::stream;
        Box::pin(stream::iter(vec![
            Ok(StreamChunk::ToolCallDelta {
                index: 0,
                id: CallId("c1".into()),
                name: Some("boom".into()),
                arguments_delta: "{}".into(),
            }),
            Ok(StreamChunk::BlockEnd {
                index: 0,
                block: starriver_kernel::llm::ContentBlock::ToolCall {
                    id: CallId("c1".into()),
                    name: Some("boom".into()),
                    arguments: JsonValue::Object(Vec::new()),
                },
            }),
            Ok(StreamChunk::Finish {
                reason: FinishReason::ToolCalls,
                replay_state: None,
            }),
        ]))
    }
}

/// Tools whose prepare always fails (B3 scheduler failure).
struct FailingTools;

impl ToolRuntimeView for FailingTools {
    fn schemas(&self) -> Vec<ToolSchema> {
        Vec::new()
    }
    fn schemas_for(&self, _agent_id: Option<&str>) -> Vec<ToolSchema> {
        Vec::new()
    }
    fn sdk_schemas(&self) -> Vec<ToolSchema> {
        Vec::new()
    }
    fn is_ptc_active(&self) -> bool {
        false
    }
    fn restrict_for(
        &self,
        _agent_id: &str,
        _allow: Vec<String>,
    ) -> Result<cordis_rslite::EffectHandle, ToolError> {
        Ok(cordis_rslite::EffectHandle::none())
    }
    fn execution_mode(&self, _input: &ToolExecutionInput) -> ToolExecutionMode {
        ToolExecutionMode::Exclusive
    }
    fn prepare(
        &self,
        _input: ToolExecutionInput,
    ) -> cordis_rslite::BoxFuture<
        Result<starriver_kernel::tools::ScheduledToolPreparation, ToolError>,
    > {
        Box::pin(async { Err(ToolError::NotFound("boom".into())) })
    }
    fn dispatch(
        &self,
        _exec: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<starriver_kernel::tools::ScheduledToolDispatch, ToolError>>
    {
        Box::pin(async { Err(ToolError::NotFound("unused".into())) })
    }
    fn finalize(
        &self,
        _exec: ToolRunContext,
        _result: ToolExecutionResult,
    ) -> cordis_rslite::BoxFuture<Result<ToolExecutionResult, ToolError>> {
        Box::pin(async { Err(ToolError::NotFound("unused".into())) })
    }
    fn finish(
        &self,
        _exec: ToolRunContext,
        _result: ToolExecutionResult,
    ) -> Result<ToolExecutionResult, ToolError> {
        Err(ToolError::NotFound("unused".into()))
    }

    fn presentation_meta(
        &self,
        _input: &ToolExecutionInput,
        _value: &cordis_rslite::Value,
    ) -> Option<serde_json::Value> {
        None
    }
}

/// An llm view whose first stream panics (B2 driver containment).
struct PanicLlm {
    first: AtomicBool,
}

impl LlmRuntimeView for PanicLlm {
    fn prepare_call(
        &self,
        _config: LlmCallConfig,
        _cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> cordis_rslite::BoxFuture<Result<PreparedLlmCall, LlmError>> {
        Box::pin(async { Err(LlmError::NoAdapter("mock".into())) })
    }
    fn stream(
        &self,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
        use futures::stream;
        if self.first.swap(false, Ordering::SeqCst) {
            Box::pin(stream::once(async { panic!("integration panic") }))
        } else {
            Box::pin(stream::iter(vec![
                Ok(StreamChunk::TextDelta {
                    index: 0,
                    text: "ok".into(),
                }),
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: starriver_kernel::llm::ContentBlock::Text("ok".into()),
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                }),
            ]))
        }
    }
}

struct LoopHarness {
    registry: Arc<dyn AgentRegistry>,
    sessions: Arc<dyn SessionStoreView>,
}

impl LoopHarness {
    fn new(tools: Arc<dyn ToolRuntimeView>, llm: Arc<dyn LlmRuntimeView>) -> Self {
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let sessions: Arc<dyn SessionStoreView> = Arc::new(
            starriver_session::InMemorySessionStore::new(bus, ctx.clone()),
        );
        let registry = Arc::new(InMemoryAgentRegistry::new());
        let loop_service = AgentLoop::new(
            ctx,
            AgentLoopConfig::default(),
            Arc::clone(&sessions),
            tools,
            llm,
            Arc::clone(&registry) as Arc<dyn AgentRegistry>,
            None,
            std::collections::HashMap::new(),
            Arc::new(starriver_kernel::agent::AgentScopeRegistry::new()),
            Arc::new(starriver_kernel::system_prompt::SystemPromptRegistry::new()),
        );
        let _factory = registry.set_factory(Arc::new(loop_service)).unwrap();
        Self { registry, sessions }
    }

    async fn create(&self, id: &str) -> starriver_kernel::agent::AgentHandle {
        self.registry
            .create(CreateAgentOptions {
                session_id: SessionId(id.into()),
                seed: Vec::new(),
                cwd: None,
                agent_preset: None,
                agent_options: AgentOptions {
                    compaction_tuning: None,
                    provider: Some("mock".into()),
                    model: Some("m1".into()),
                    ..Default::default()
                },
            })
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn integration_cancel_then_followup_resumes_with_fresh_turn() {
    let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
        EventBus::new(),
        Context::new(),
    )) as Arc<dyn ToolRuntimeView>;
    let bus = EventBus::new();
    let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
    let _adapter = llm
        .register_adapter(Arc::new(CancelThenStopAdapter {
            providers: vec!["mock".into()],
            first: AtomicBool::new(true),
        }))
        .unwrap();
    let harness = LoopHarness::new(tools, llm);
    let handle = harness.create("b1").await;

    handle.agent.followup(message("a"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    handle.agent.followup(message("b"));
    handle.agent.cancel(
        starriver_kernel::agent::AgentCancelCause::new(
            starriver_kernel::agent::AgentCancelKind::User,
        ),
        true,
    );
    let idle =
        tokio::time::timeout(std::time::Duration::from_secs(5), handle.agent.when_idle()).await;
    assert!(idle.is_ok(), "cancel + pending followup must converge");

    let events = handle.agent.session().events();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == "turn/start")
            .count(),
        1
    );
    assert_eq!(handle.agent.pending_messages().next_turn.len(), 1);

    handle.agent.followup(message("c"));
    let idle =
        tokio::time::timeout(std::time::Duration::from_secs(5), handle.agent.when_idle()).await;
    assert!(idle.is_ok(), "waking send must resume the parked queue");
    let events = handle.agent.session().events();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == "turn/start")
            .count(),
        3
    );
    assert_eq!(handle.agent.pending_messages().next_turn.len(), 0);
    let last_turn_end = events
        .iter()
        .rev()
        .find(|e| e.event_type == "turn/end")
        .expect("turn/end");
    assert_eq!(
        last_turn_end
            .data
            .get("reason")
            .and_then(|r| r.as_str_at("kind")),
        Some("completed")
    );
    handle.dispose();
}

#[tokio::test]
async fn integration_scheduler_failure_ends_turn_with_error() {
    let tools = Arc::new(FailingTools) as Arc<dyn ToolRuntimeView>;
    let bus = EventBus::new();
    let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
    let _adapter = llm
        .register_adapter(Arc::new(ToolCallAdapter {
            providers: vec!["mock".into()],
        }))
        .unwrap();
    let harness = LoopHarness::new(tools, llm);
    let handle = harness.create("b3").await;

    let errors = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&errors);
    let _listener = handle.agent.ctx().on::<AgentErrorEvent, _>(move |_| {
        let counter = Arc::clone(&counter);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(cordis_rslite::Value::new(()))
        })
    });

    handle.agent.followup(message("hi"));
    let idle =
        tokio::time::timeout(std::time::Duration::from_secs(5), handle.agent.when_idle()).await;
    assert!(idle.is_ok());
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    assert_eq!(errors.load(Ordering::SeqCst), 1, "agent/error must fire");
    let events = handle.agent.session().events();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == "tool/result")
            .count(),
        0,
        "scheduler failure must not fabricate tool results"
    );
    let turn_end = events
        .iter()
        .find(|e| e.event_type == "turn/end")
        .expect("turn/end");
    assert_eq!(
        turn_end
            .data
            .get("reason")
            .and_then(|r| r.as_str_at("kind")),
        Some("error")
    );
    handle.dispose();
}

#[tokio::test]
async fn integration_driver_panic_is_contained() {
    let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
        EventBus::new(),
        Context::new(),
    )) as Arc<dyn ToolRuntimeView>;
    let llm = Arc::new(PanicLlm {
        first: AtomicBool::new(true),
    }) as Arc<dyn LlmRuntimeView>;
    let harness = LoopHarness::new(tools, llm);
    let handle = harness.create("b2").await;

    let errors = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&errors);
    let _listener = handle.agent.ctx().on::<AgentErrorEvent, _>(move |_| {
        let counter = Arc::clone(&counter);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(cordis_rslite::Value::new(()))
        })
    });

    handle.agent.followup(message("a"));
    let idle =
        tokio::time::timeout(std::time::Duration::from_secs(5), handle.agent.when_idle()).await;
    assert!(idle.is_ok(), "driver must survive the panic");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(errors.load(Ordering::SeqCst), 1, "panic must be reported");

    handle.agent.followup(message("b"));
    let idle =
        tokio::time::timeout(std::time::Duration::from_secs(5), handle.agent.when_idle()).await;
    assert!(idle.is_ok(), "second turn must run after the panic");
    let events = handle.agent.session().events();
    let last_turn_end = events
        .iter()
        .rev()
        .find(|e| e.event_type == "turn/end")
        .expect("turn/end");
    assert_eq!(
        last_turn_end
            .data
            .get("reason")
            .and_then(|r| r.as_str_at("kind")),
        Some("completed")
    );
    handle.dispose();
}

#[tokio::test]
async fn integration_dispose_after_cancel_tears_down() {
    let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
        EventBus::new(),
        Context::new(),
    )) as Arc<dyn ToolRuntimeView>;
    let bus = EventBus::new();
    let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
    let _adapter = llm
        .register_adapter(Arc::new(CancelThenStopAdapter {
            providers: vec!["mock".into()],
            first: AtomicBool::new(true),
        }))
        .unwrap();
    let harness = LoopHarness::new(tools, llm);
    let handle = harness.create("b4").await;

    handle.agent.followup(message("a"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    handle.agent.cancel(
        starriver_kernel::agent::AgentCancelCause::new(
            starriver_kernel::agent::AgentCancelKind::User,
        ),
        false,
    );
    let idle =
        tokio::time::timeout(std::time::Duration::from_secs(5), handle.agent.when_idle()).await;
    assert!(idle.is_ok());

    handle.dispose();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        harness.registry.get(&SessionId("b4".into())).is_none(),
        "agent registry entry must be detached"
    );
    assert!(
        harness.sessions.get(&SessionId("b4".into())).is_none(),
        "session entry must be removed"
    );
}

#[tokio::test]
async fn integration_duplicate_send_panics_without_damage() {
    let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
        EventBus::new(),
        Context::new(),
    )) as Arc<dyn ToolRuntimeView>;
    let bus = EventBus::new();
    let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
    let _adapter = llm
        .register_adapter(Arc::new(StopAdapter {
            providers: vec!["mock".into()],
        }))
        .unwrap();
    let harness = LoopHarness::new(tools, llm);
    let handle = harness.create("w1").await;

    handle.agent.followup(message("a"));
    handle.agent.followup(message("b"));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle.agent.followup(message("a"));
    }));
    assert!(result.is_err(), "duplicate pending id must panic on send");

    let events = handle.agent.session().events();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == "agent/inbox/spliced")
            .count(),
        2
    );

    handle.agent.followup(message("c"));
    let idle =
        tokio::time::timeout(std::time::Duration::from_secs(5), handle.agent.when_idle()).await;
    assert!(idle.is_ok(), "agent must survive a duplicate send");
    handle.dispose();
}

#[tokio::test]
async fn integration_max_tokens_is_sticky() {
    let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
        EventBus::new(),
        Context::new(),
    )) as Arc<dyn ToolRuntimeView>;
    let bus = EventBus::new();
    let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
    let _adapter = llm
        .register_adapter(Arc::new(MaxTokensThenStopAdapter {
            providers: vec!["mock".into()],
            calls: AtomicUsize::new(0),
        }))
        .unwrap();
    let harness = LoopHarness::new(tools, llm);
    let handle = harness.create("w2").await;

    handle.agent.followup(message("a"));
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    handle.agent.steer(message("b"));
    let idle =
        tokio::time::timeout(std::time::Duration::from_secs(5), handle.agent.when_idle()).await;
    assert!(idle.is_ok());

    let events = handle.agent.session().events();
    let turn_end = events
        .iter()
        .find(|e| e.event_type == "turn/end")
        .expect("turn/end");
    assert_eq!(
        turn_end
            .data
            .get("reason")
            .and_then(|r| r.as_str_at("kind")),
        Some("max-tokens"),
        "later completed steps must not downgrade max-tokens"
    );
    handle.dispose();
}

#[tokio::test]
async fn integration_turn_stopping_can_extend() {
    use starriver_kernel::agent::AgentTurnStopping;

    let tools = Arc::new(starriver_tools::InMemoryToolRuntime::new(
        EventBus::new(),
        Context::new(),
    )) as Arc<dyn ToolRuntimeView>;
    let bus = EventBus::new();
    let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
    let _adapter = llm
        .register_adapter(Arc::new(StopAdapter {
            providers: vec!["mock".into()],
        }))
        .unwrap();
    let harness = LoopHarness::new(tools, llm);
    let handle = harness.create("w3").await;

    let injected = Arc::new(AtomicBool::new(false));
    let steer_target = Arc::clone(&handle.agent);
    let injected_flag = Arc::clone(&injected);
    let _hook = handle
        .agent
        .ctx()
        .on_sync::<AgentTurnStopping, _>(move |_| {
            if !injected_flag.swap(true, Ordering::SeqCst) {
                steer_target.steer(message("extra"));
            }
            Ok(cordis_rslite::Value::new(()))
        });
    handle.agent.followup(message("a"));
    let idle =
        tokio::time::timeout(std::time::Duration::from_secs(5), handle.agent.when_idle()).await;
    assert!(idle.is_ok());

    let events = handle.agent.session().events();
    assert!(
        events
            .iter()
            .filter(|e| e.event_type == "user/message")
            .any(|e| e.data.as_str_at("id") == Some("extra")),
        "turn-stopping injected input must be consumed"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == "assistant/message")
            .count(),
        2
    );
    handle.dispose();
}

#[tokio::test]
async fn integration_proposal_chain_preserves_edits() {
    use starriver_kernel::agent::AgentRequest;

    let tools_runtime = starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let _h1 = tools_runtime
        .register(starriver_kernel::tools::ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: "echo".into(),
            description: "echo".into(),
            parameters: cordis_rslite::Value::new(()),
            output: cordis_rslite::Value::new(()),
            render: Arc::new(|_, value| {
                vec![starriver_kernel::llm::ContentBlock::Text(format!(
                    "{value:?}"
                ))]
            }),
            executor: Arc::new(Echo),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        })
        .unwrap();
    let tools = Arc::new(tools_runtime) as Arc<dyn ToolRuntimeView>;
    let bus = EventBus::new();
    let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(bus.clone()));
    let _adapter = llm
        .register_adapter(Arc::new(EchoToolThenStopAdapter {
            providers: vec!["mock".into()],
            calls: AtomicUsize::new(0),
        }))
        .unwrap();
    let harness = LoopHarness::new(tools, llm);
    let handle = harness.create("w4").await;

    let seen = Arc::new(std::sync::Mutex::new(Vec::<Option<f64>>::new()));
    let recorder = Arc::clone(&seen);
    let _request = handle
        .agent
        .ctx()
        .on_waterfall::<AgentRequest, _>(move |_event, next| {
            let recorder = Arc::clone(&recorder);
            Box::pin(async move {
                let proposed = next().await.unwrap();
                recorder.lock().unwrap().push(proposed.temperature);
                Ok(starriver_kernel::llm::LlmCallConfig {
                    temperature: Some(0.5),
                    ..proposed
                })
            })
        });
    handle.agent.followup(message("a"));
    let idle =
        tokio::time::timeout(std::time::Duration::from_secs(5), handle.agent.when_idle()).await;
    assert!(idle.is_ok());

    let proposals = seen.lock().unwrap().clone();
    assert_eq!(
        proposals.len(),
        2,
        "a tool turn produces two model requests"
    );
    assert_eq!(proposals[0], None);
    assert_eq!(
        proposals[1],
        Some(0.5),
        "the listener's edit must survive the proposal chain"
    );
    handle.dispose();
}

struct Echo;

impl starriver_kernel::tools::ToolExecutor for Echo {
    fn execute(
        &self,
        arguments: cordis_rslite::Value,
        _ctx: starriver_kernel::tools::ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<cordis_rslite::Value, starriver_kernel::tools::ToolError>>
    {
        Box::pin(async { Ok(arguments) })
    }
}
