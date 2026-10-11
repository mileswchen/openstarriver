//! End-to-end agent tests with a scripted mock model: the adapter records the
//! full request (system / tools / derived messages) and returns scripted
//! streams, so the whole loop (public AgentLoop + registry + in-memory
//! services) is exercised against controlled model input/output.

use starriver_kernel::json::JsonValue;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cordis_rslite::{Context, EventBus};
use futures::StreamExt;
use starriver_agent_loop::agent_loop::{AgentLoop, AgentLoopConfig};
use starriver_agent_loop::registry::InMemoryAgentRegistry;
use starriver_agent_loop::services::{SessionStoreView, ToolRuntimeView};
use starriver_kernel::agent::{Agent, AgentOptions, AgentRegistry, CreateAgentOptions};
use starriver_kernel::llm::{
    CallId, FinishReason, GenerateOptions, LlmAdapter, LlmError, LlmRuntime, Message, StreamChunk,
};
use starriver_kernel::session::SessionId;
use starriver_kernel::tools::ToolRuntime;

fn message(text: &str) -> Message {
    Message {
        id: starriver_kernel::llm::MessageId(text.into()),
        role: starriver_kernel::llm::MessageRole::User,
        content: vec![starriver_kernel::llm::ContentBlock::Text(text.into())],
        source: starriver_kernel::llm::MessageSource::User,
    }
}

fn text_of(message: &Message) -> Option<&str> {
    match message.content.first()? {
        starriver_kernel::llm::ContentBlock::Text(text) => Some(text),
        _ => None,
    }
}

fn tool_call_block(id: &str, name: &str) -> starriver_kernel::llm::ContentBlock {
    starriver_kernel::llm::ContentBlock::ToolCall {
        id: CallId(id.into()),
        name: Some(name.into()),
        arguments: JsonValue::Object(Vec::new()),
    }
}

/// One scripted model response for a single stream call.
#[derive(Clone)]
enum ScriptedOutput {
    Text(String),
    ToolCalls(Vec<(String, String)>),
    /// Text content followed by a max-tokens finish (threshold hit).
    TextThenMaxTokens(String),
    /// Tool calls then a max-tokens finish (truncation drops them).
    ToolCallsThenMaxTokens(Vec<(String, String)>),
    /// A text delta that never gets its block-end.
    UnclosedText(String),
    /// Text that ends the stream without a finish chunk.
    TextNoFinish(String),
    /// Partial text, then a terminal stream error.
    TextThenError {
        text: String,
        code: String,
        message: String,
    },
    MaxTokens,
    Error {
        code: String,
        message: String,
    },
    /// Yield a partial delta, then end when the request is cancelled.
    Hold,
    /// End the stream with no chunks at all.
    Empty,
}

/// Full request input observed by the mock model.
#[derive(Debug, Clone)]
struct RecordedRequest {
    provider: String,
    model: String,
    system: Option<String>,
    tool_names: Vec<String>,
    messages: Vec<Message>,
    session_id: Option<String>,
    max_tokens: Option<u64>,
}

struct ScriptedAdapter {
    providers: Vec<String>,
    script: Arc<Mutex<VecDeque<ScriptedOutput>>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl ScriptedAdapter {
    fn new(script: Vec<ScriptedOutput>) -> (Arc<Self>, Arc<Mutex<Vec<RecordedRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let adapter = Arc::new(Self {
            providers: vec!["mock".into()],
            script: Arc::new(Mutex::new(script.into())),
            requests: Arc::clone(&requests),
        });
        (adapter, requests)
    }
}

impl LlmAdapter for ScriptedAdapter {
    fn providers(&self) -> &[String] {
        &self.providers
    }
    fn stream(
        &self,
        options: GenerateOptions,
    ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
        use futures::stream;
        self.requests.lock().unwrap().push(RecordedRequest {
            provider: options.provider.clone(),
            model: options.model.clone(),
            system: options.system.clone(),
            tool_names: options
                .tools
                .as_ref()
                .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
                .unwrap_or_default(),
            messages: options.messages.clone(),
            session_id: options.session_id.clone(),
            max_tokens: options.max_tokens,
        });
        let output = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ScriptedOutput::Text("fallback".into()));
        match output {
            ScriptedOutput::Text(text) => {
                let text_for_block = text.clone();
                Box::pin(stream::iter(vec![
                    Ok(StreamChunk::TextDelta { index: 0, text }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::Text(text_for_block),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]))
            }
            ScriptedOutput::ToolCalls(calls) => {
                let mut chunks = Vec::new();
                for (index, (call_id, name)) in calls.iter().enumerate() {
                    chunks.push(Ok(StreamChunk::ToolCallDelta {
                        index: index as u64,
                        id: CallId(call_id.clone()),
                        name: Some(name.clone()),
                        arguments_delta: "{}".into(),
                    }));
                    chunks.push(Ok(StreamChunk::BlockEnd {
                        index: index as u64,
                        block: tool_call_block(call_id, name),
                    }));
                }
                chunks.push(Ok(StreamChunk::Finish {
                    reason: FinishReason::ToolCalls,
                    replay_state: None,
                }));
                Box::pin(stream::iter(chunks))
            }
            ScriptedOutput::MaxTokens => Box::pin(stream::iter(vec![Ok(StreamChunk::Finish {
                reason: FinishReason::MaxTokens,
                replay_state: None,
            })])),
            ScriptedOutput::TextThenMaxTokens(text) => {
                let text_for_block = text.clone();
                Box::pin(stream::iter(vec![
                    Ok(StreamChunk::TextDelta { index: 0, text }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::Text(text_for_block),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::MaxTokens,
                        replay_state: None,
                    }),
                ]))
            }
            ScriptedOutput::ToolCallsThenMaxTokens(calls) => {
                let mut chunks = Vec::new();
                for (index, (call_id, name)) in calls.iter().enumerate() {
                    chunks.push(Ok(StreamChunk::ToolCallDelta {
                        index: index as u64,
                        id: CallId(call_id.clone()),
                        name: Some(name.clone()),
                        arguments_delta: "{}".into(),
                    }));
                    chunks.push(Ok(StreamChunk::BlockEnd {
                        index: index as u64,
                        block: tool_call_block(call_id, name),
                    }));
                }
                chunks.push(Ok(StreamChunk::Finish {
                    reason: FinishReason::MaxTokens,
                    replay_state: None,
                }));
                Box::pin(stream::iter(chunks))
            }
            ScriptedOutput::UnclosedText(text) => Box::pin(stream::iter(vec![
                Ok(StreamChunk::TextDelta { index: 0, text }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                }),
            ])),
            ScriptedOutput::TextNoFinish(text) => {
                let text_for_block = text.clone();
                Box::pin(stream::iter(vec![
                    Ok(StreamChunk::TextDelta { index: 0, text }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: starriver_kernel::llm::ContentBlock::Text(text_for_block),
                    }),
                ]))
            }
            ScriptedOutput::TextThenError {
                text,
                code,
                message,
            } => Box::pin(stream::iter(vec![
                Ok(StreamChunk::TextDelta { index: 0, text }),
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: starriver_kernel::llm::ContentBlock::Text("partial-fail".into()),
                }),
                Err(LlmError::Call {
                    code,
                    message,
                    status: Some(500),
                    provider_retry_after_ms: None,
                    request_id: None,
                }),
            ])),
            ScriptedOutput::Error { code, message } => {
                Box::pin(stream::iter(vec![Err(LlmError::Call {
                    code,
                    message,
                    status: Some(429),
                    provider_retry_after_ms: None,
                    request_id: None,
                })]))
            }
            ScriptedOutput::Hold => {
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
            }
            // BUG-99: a legitimately empty model response still carries the
            // terminal finish chunk — a stream without one is a transport
            // truncation and now fails the turn instead of completing.
            ScriptedOutput::Empty => Box::pin(futures::stream::iter(vec![Ok(
                starriver_kernel::llm::StreamChunk::Finish {
                    reason: starriver_kernel::llm::FinishReason::Stop,
                    replay_state: None,
                },
            )])),
        }
    }
}

struct E2E {
    bus: EventBus,
    registry: Arc<dyn AgentRegistry>,
    sessions: Arc<dyn SessionStoreView>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl E2E {
    fn with_sessions(
        script: Vec<ScriptedOutput>,
        tools: Arc<dyn ToolRuntimeView>,
        sessions: Arc<dyn SessionStoreView>,
    ) -> Self {
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let harness_bus = bus.clone();
        let registry = Arc::new(InMemoryAgentRegistry::new());
        let (adapter, requests) = ScriptedAdapter::new(script);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(EventBus::new()));
        let _adapter = llm.register_adapter(adapter).unwrap();
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
        Self {
            bus: harness_bus,
            registry,
            sessions,
            requests,
        }
    }

    fn new(script: Vec<ScriptedOutput>, tools: Arc<dyn ToolRuntimeView>) -> Self {
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let harness_bus = bus.clone();
        let sessions: Arc<dyn SessionStoreView> = Arc::new(
            starriver_session::InMemorySessionStore::new(bus, ctx.clone()),
        );
        let registry = Arc::new(InMemoryAgentRegistry::new());
        let (adapter, requests) = ScriptedAdapter::new(script);
        let llm = Arc::new(starriver_llm::InMemoryLlmRuntime::new(EventBus::new()));
        let _adapter = llm.register_adapter(adapter).unwrap();
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
        Self {
            bus: harness_bus,
            registry,
            sessions,
            requests,
        }
    }

    async fn create_with(
        &self,
        id: &str,
        options: AgentOptions,
    ) -> starriver_kernel::agent::AgentHandle {
        self.registry
            .create(CreateAgentOptions {
                session_id: SessionId(id.into()),
                seed: Vec::new(),
                cwd: None,
                agent_options: options,
    agent_preset: None,
        })
            .await
            .unwrap()
    }

    async fn create(&self, id: &str) -> starriver_kernel::agent::AgentHandle {
        self.create_with(
            id,
            AgentOptions {
                compaction_tuning: None,
                provider: Some("mock".into()),
                model: Some("m1".into()),
                max_tokens: Some(512),
                system: Some("You are a helpful agent.".into()),
                ..Default::default()
            },
        )
        .await
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

fn empty_tools() -> Arc<dyn ToolRuntimeView> {
    Arc::new(starriver_tools::InMemoryToolRuntime::new(
        EventBus::new(),
        Context::new(),
    )) as Arc<dyn ToolRuntimeView>
}

async fn idle(agent: &Arc<dyn Agent>) {
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), agent.when_idle()).await;
    assert!(result.is_ok(), "driver must converge");
}

fn message_texts(messages: &[Message]) -> Vec<(String, String)> {
    messages
        .iter()
        .map(|message| {
            (
                format!("{:?}", message.role),
                text_of(message).unwrap_or("").to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn e2e_multiturn_history_is_derived() {
    let e2e = E2E::new(
        vec![
            ScriptedOutput::Text("hello from model".into()),
            ScriptedOutput::Text("second reply".into()),
        ],
        empty_tools(),
    );
    let handle = e2e.create("e2e-history").await;
    handle.agent.followup(message("hi"));
    idle(&handle.agent).await;
    handle.agent.followup(message("again"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].provider, "mock");
    assert_eq!(requests[0].model, "m1");
    assert_eq!(
        requests[0].system.as_deref(),
        Some("You are a helpful agent.")
    );
    assert_eq!(requests[0].session_id.as_deref(), Some("e2e-history"));
    assert_eq!(requests[0].max_tokens, Some(512));
    // Request 1: only the first user message.
    assert_eq!(
        message_texts(&requests[0].messages),
        vec![("User".into(), "hi".into())]
    );
    // Request 2: prior history (user + assistant) plus the new prompt.
    assert_eq!(
        message_texts(&requests[1].messages),
        vec![
            ("User".into(), "hi".into()),
            ("Assistant".into(), "hello from model".into()),
            ("User".into(), "again".into()),
        ]
    );

    let events = handle.agent.session().events();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == "turn/start")
            .count(),
        2
    );
    handle.dispose();
}

#[tokio::test]
async fn e2e_tool_call_flow_passes_tool_result_back() {
    let tools_runtime = starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let _h = tools_runtime
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
    let e2e = E2E::new(
        vec![
            ScriptedOutput::ToolCalls(vec![("c1".into(), "echo".into())]),
            ScriptedOutput::Text("done".into()),
        ],
        Arc::new(tools_runtime) as Arc<dyn ToolRuntimeView>,
    );
    let handle = e2e.create("e2e-tool").await;
    handle.agent.followup(message("run echo"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(
        requests.len(),
        2,
        "tool turn must produce two model requests"
    );
    // Request 1 presents the echo schema.
    assert_eq!(requests[0].tool_names, vec!["echo"]);
    // Request 2 contains assistant tool-call + tool result in history.
    let second = &requests[1].messages;
    let has_tool_call = second.iter().any(|m| {
        m.role == starriver_kernel::llm::MessageRole::Assistant
            && matches!(
                m.content.first(),
                Some(starriver_kernel::llm::ContentBlock::ToolCall { name: Some(n), .. }) if n == "echo"
            )
    });
    let has_tool_result = second.iter().any(|m| {
        m.role == starriver_kernel::llm::MessageRole::User
            && matches!(
                m.content.first(),
                Some(starriver_kernel::llm::ContentBlock::ToolResult { .. })
            )
    });
    assert!(
        has_tool_call,
        "assistant tool-call must be in the next request"
    );
    assert!(has_tool_result, "tool result must be in the next request");

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
        Some("completed")
    );
    handle.dispose();
}

#[tokio::test]
async fn e2e_parallel_tools_commit_in_model_order() {
    use std::time::Duration;

    struct Slow;
    impl starriver_kernel::tools::ToolExecutor for Slow {
        fn execute(
            &self,
            arguments: cordis_rslite::Value,
            _ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<
            Result<cordis_rslite::Value, starriver_kernel::tools::ToolError>,
        > {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(40)).await;
                Ok(arguments)
            })
        }
    }
    struct Fast;
    impl starriver_kernel::tools::ToolExecutor for Fast {
        fn execute(
            &self,
            arguments: cordis_rslite::Value,
            _ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<
            Result<cordis_rslite::Value, starriver_kernel::tools::ToolError>,
        > {
            Box::pin(async { Ok(arguments) })
        }
    }

    fn parallel_definition(
        name: &str,
        executor: Arc<dyn starriver_kernel::tools::ToolExecutor>,
    ) -> starriver_kernel::tools::ToolDefinition {
        starriver_kernel::tools::ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: name.into(),
            description: name.into(),
            parameters: cordis_rslite::Value::new(()),
            output: cordis_rslite::Value::new(()),
            render: Arc::new(|_, value| {
                vec![starriver_kernel::llm::ContentBlock::Text(format!(
                    "{value:?}"
                ))]
            }),
            executor,
            is_concurrency_safe: Some(Arc::new(|_| true)),
            timeout: None,
            finalize_content: None,
        }
    }

    let tools_runtime = starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let _h1 = tools_runtime
        .register(parallel_definition("slow", Arc::new(Slow)))
        .unwrap();
    let _h2 = tools_runtime
        .register(parallel_definition("fast", Arc::new(Fast)))
        .unwrap();
    let e2e = E2E::new(
        vec![
            ScriptedOutput::ToolCalls(vec![
                ("c1".into(), "slow".into()),
                ("c2".into(), "fast".into()),
            ]),
            ScriptedOutput::Text("both done".into()),
        ],
        Arc::new(tools_runtime) as Arc<dyn ToolRuntimeView>,
    );
    let handle = e2e.create("e2e-parallel").await;
    handle.agent.followup(message("run both"));
    idle(&handle.agent).await;

    let events = handle.agent.session().events();
    let results = events
        .iter()
        .filter(|e| e.event_type == "tool/result")
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    // Model-order commit: c1 (slow) must be committed before c2 (fast), even
    // though c2 settles first.
    let call_ids = results
        .iter()
        .map(|result| {
            result
                .data
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|content| content.as_array())
                .and_then(|blocks| blocks.first())
                .and_then(|block| block.as_str_at("toolCallId"))
                .unwrap_or("")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(call_ids, vec!["c1", "c2"]);

    let turn_end = events
        .iter()
        .find(|e| e.event_type == "turn/end")
        .expect("turn/end");
    assert_eq!(
        turn_end
            .data
            .get("reason")
            .and_then(|r| r.as_str_at("kind")),
        Some("completed")
    );
    handle.dispose();
}

#[tokio::test(start_paused = true)]
async fn e2e_request_error_retry_recovers() {
    use starriver_kernel::agent::{AgentRequestError, RequestErrorAction};

    let e2e = E2E::new(
        vec![
            ScriptedOutput::Error {
                code: "RATE_LIMIT".into(),
                message: "slow down".into(),
            },
            ScriptedOutput::Text("recovered".into()),
        ],
        empty_tools(),
    );
    let handle = e2e.create("e2e-retry").await;
    let _retry = handle
        .agent
        .ctx()
        .on_waterfall::<AgentRequestError, _>(|_, _next| {
            Box::pin(async { Ok(RequestErrorAction::Retry) })
        });
    handle.agent.followup(message("hi"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(requests.len(), 2, "failed call must be retried");
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
        Some("completed")
    );
    handle.dispose();
}

#[tokio::test]
async fn e2e_cancel_mid_stream_then_followup_resumes() {
    let e2e = E2E::new(
        vec![ScriptedOutput::Hold, ScriptedOutput::Text("resumed".into())],
        empty_tools(),
    );
    let handle = e2e.create("e2e-cancel").await;
    handle.agent.followup(message("a"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    handle.agent.cancel(
        starriver_kernel::agent::AgentCancelCause::new(
            starriver_kernel::agent::AgentCancelKind::User,
        ),
        false,
    );
    idle(&handle.agent).await;

    let events = handle.agent.session().events();
    let first_turn_end = events
        .iter()
        .find(|e| e.event_type == "turn/end")
        .expect("turn/end");
    assert_eq!(
        first_turn_end
            .data
            .get("reason")
            .and_then(|r| r.as_str_at("kind")),
        Some("aborted")
    );
    let anchor = events
        .iter()
        .find(|e| e.event_type == "assistant/message")
        .expect("interrupted anchor");
    assert_eq!(
        anchor
            .data
            .get("interrupted")
            .and_then(starriver_kernel::json::JsonValue::as_bool),
        Some(true)
    );

    handle.agent.followup(message("b"));
    idle(&handle.agent).await;
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
        Some("completed"),
        "a fresh followup must complete after cancellation"
    );
    handle.dispose();
}

#[tokio::test]
async fn e2e_max_tokens_is_sticky_with_steering() {
    let e2e = E2E::new(
        vec![
            ScriptedOutput::MaxTokens,
            ScriptedOutput::Text("after".into()),
        ],
        empty_tools(),
    );
    let handle = e2e.create("e2e-max-tokens").await;
    handle.agent.followup(message("a"));
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    handle.agent.steer(message("b"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(requests.len(), 2, "steering keeps the turn running");
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
        Some("max-tokens")
    );
    handle.dispose();
}

#[tokio::test]
async fn e2e_steer_lands_in_next_step_during_tool_turn() {
    use std::time::Duration;

    struct SlowEcho;
    impl starriver_kernel::tools::ToolExecutor for SlowEcho {
        fn execute(
            &self,
            arguments: cordis_rslite::Value,
            _ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<
            Result<cordis_rslite::Value, starriver_kernel::tools::ToolError>,
        > {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(60)).await;
                Ok(arguments)
            })
        }
    }

    let tools_runtime = starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let _h = tools_runtime
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
            executor: Arc::new(SlowEcho),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        })
        .unwrap();
    let e2e = E2E::new(
        vec![
            ScriptedOutput::ToolCalls(vec![("c1".into(), "echo".into())]),
            ScriptedOutput::Text("final".into()),
        ],
        Arc::new(tools_runtime) as Arc<dyn ToolRuntimeView>,
    );
    let handle = e2e.create("e2e-steer").await;
    handle.agent.followup(message("run echo"));
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    handle.agent.steer(message("steering context"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(requests.len(), 2);
    let second = &requests[1].messages;
    assert!(
        message_texts(second)
            .iter()
            .any(|(role, text)| role == "User" && text == "steering context"),
        "steer must be claimed by the next step of the same turn"
    );
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
        Some("completed")
    );
    handle.dispose();
}

fn assistant_texts(events: &[starriver_kernel::session::SessionEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.event_type == "assistant/message")
        .filter_map(|e| {
            e.data
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|content| content.as_array())
                .and_then(|blocks| blocks.first())
                .and_then(|block| block.as_str_at("text"))
                .map(String::from)
        })
        .collect()
}

fn turn_end_kind(events: &[starriver_kernel::session::SessionEvent]) -> Option<String> {
    events
        .iter()
        .find(|e| e.event_type == "turn/end")
        .and_then(|e| e.data.get("reason"))
        .and_then(|reason| reason.as_str_at("kind"))
        .map(String::from)
}

#[tokio::test]
async fn e2e_max_tokens_threshold_keeps_partial_text() {
    let e2e = E2E::new(
        vec![ScriptedOutput::TextThenMaxTokens("partial answer".into())],
        empty_tools(),
    );
    let handle = e2e.create("e2e-max-text").await;
    handle.agent.followup(message("a"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(requests.len(), 1, "max-tokens closes without another step");
    let events = handle.agent.session().events();
    assert_eq!(
        assistant_texts(&events),
        vec!["partial answer".to_string()],
        "max-tokens keeps the text already produced"
    );
    assert_eq!(turn_end_kind(&events).as_deref(), Some("max-tokens"));
    handle.dispose();
}

#[tokio::test]
async fn e2e_max_tokens_threshold_truncates_tool_calls() {
    let tools_runtime = starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let _h = tools_runtime
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
    let e2e = E2E::new(
        vec![ScriptedOutput::ToolCallsThenMaxTokens(vec![(
            "c1".into(),
            "echo".into(),
        )])],
        Arc::new(tools_runtime) as Arc<dyn ToolRuntimeView>,
    );
    let handle = e2e.create("e2e-max-tool").await;
    handle.agent.followup(message("a"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(
        requests.len(),
        1,
        "truncated tool calls must not be dispatched"
    );
    let events = handle.agent.session().events();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event_type == "tool/call")
            .count(),
        0,
        "max-tokens truncation drops tool calls before dispatch"
    );
    assert_eq!(turn_end_kind(&events).as_deref(), Some("max-tokens"));
    handle.dispose();
}

#[tokio::test]
async fn e2e_unclosed_text_block_preserves_partial() {
    let e2e = E2E::new(
        vec![ScriptedOutput::UnclosedText("part".into())],
        empty_tools(),
    );
    let handle = e2e.create("e2e-unclosed").await;
    handle.agent.followup(message("a"));
    idle(&handle.agent).await;

    let events = handle.agent.session().events();
    assert_eq!(
        assistant_texts(&events),
        vec!["part".to_string()],
        "an unclosed text block assembles from its deltas"
    );
    assert_eq!(turn_end_kind(&events).as_deref(), Some("completed"));
    handle.dispose();
}

#[tokio::test]
async fn e2e_stream_without_finish_retries_and_recovers() {
    // BUG-99 (semantic flip): a stream that ends without a finish chunk is a
    // truncated provider response — never a silent Stop. Since 2026-10-04 the
    // truncation (PROVIDER_STREAM_TRUNCATED) is retryable under the default
    // policy: the step re-asks, the partial content never enters the history,
    // and only a provider that keeps truncating fails the turn (covered by
    // e2e_transient_failure_exhausts_retries_and_errors below).
    let e2e = E2E::new(
        vec![
            ScriptedOutput::TextNoFinish("ok".into()),
            ScriptedOutput::Text("recovered".into()),
        ],
        empty_tools(),
    );
    let handle = e2e.create("e2e-no-finish").await;
    handle.agent.followup(message("a"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(requests.len(), 2, "the truncated call must be retried");
    let events = handle.agent.session().events();
    assert_eq!(
        assistant_texts(&events),
        vec!["recovered".to_string()],
        "partial content of a truncated stream must not enter the history"
    );
    assert_eq!(
        turn_end_kind(&events).as_deref(),
        Some("completed"),
        "after one truncation the retry must complete the turn"
    );
    handle.dispose();
}

#[tokio::test(start_paused = true)]
async fn e2e_transient_failure_exhausts_retries_and_errors() {
    // 2026-10-04: a transient (SERVER) failure is retried per the default
    // policy (5 retries) before the turn may die — one provider 500 used to
    // kill a 5m45s turn outright. Paused clock: the 500ms→10s backoff ladder
    // (~15.5s wall time) costs nothing.
    let failures = (0..6)
        .map(|_| ScriptedOutput::Error {
            code: "SERVER".into(),
            message: "网络错误，请稍后重试".into(),
        })
        .collect::<Vec<_>>();
    let e2e = E2E::new(failures, empty_tools());
    let handle = e2e.create("e2e-server-exhausted").await;
    handle.agent.followup(message("a"));
    // Not the shared idle() helper: its 5s convergence timeout is virtual
    // under the paused clock, and the backoff ladder alone spends ~15.5s of
    // it before the turn can end.
    handle.agent.when_idle().await;

    let requests = e2e.requests();
    assert_eq!(
        requests.len(),
        6,
        "1 initial attempt + 5 policy retries before giving up"
    );
    let events = handle.agent.session().events();
    assert_eq!(
        turn_end_kind(&events).as_deref(),
        Some("error"),
        "a persistently failing provider must end the turn visibly"
    );
    assert_eq!(
        assistant_texts(&events),
        Vec::<String>::new(),
        "no content may enter the history from failed attempts"
    );

    // The loop survives: a fresh followup completes normally (script is
    // exhausted, the mock falls back to a successful text output).
    handle.agent.followup(message("b"));
    handle.agent.when_idle().await;
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
async fn e2e_empty_stream_completes_without_content() {
    let e2e = E2E::new(vec![ScriptedOutput::Empty], empty_tools());
    let handle = e2e.create("e2e-empty").await;
    handle.agent.followup(message("a"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].messages.len(),
        1,
        "empty assistant content must not enter the derived history"
    );
    let events = handle.agent.session().events();
    assert!(assistant_texts(&events).is_empty());
    assert_eq!(turn_end_kind(&events).as_deref(), Some("completed"));
    handle.dispose();
}

#[tokio::test(start_paused = true)]
async fn e2e_error_after_partial_content_retries_cleanly() {
    use starriver_kernel::agent::{AgentRequestError, RequestErrorAction};

    let e2e = E2E::new(
        vec![
            ScriptedOutput::TextThenError {
                text: "partial-fail".into(),
                code: "SERVER".into(),
                message: "boom".into(),
            },
            ScriptedOutput::Text("recovered".into()),
        ],
        empty_tools(),
    );
    let handle = e2e.create("e2e-partial-error").await;
    let _retry = handle
        .agent
        .ctx()
        .on_waterfall::<AgentRequestError, _>(|_, _next| {
            Box::pin(async { Ok(RequestErrorAction::Retry) })
        });
    handle.agent.followup(message("a"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(requests.len(), 2, "the failed call must be retried");
    let events = handle.agent.session().events();
    assert_eq!(
        assistant_texts(&events),
        vec!["recovered".to_string()],
        "partial content from the failed call must not leak into history"
    );
    assert_eq!(turn_end_kind(&events).as_deref(), Some("completed"));
    handle.dispose();
}

#[tokio::test]
async fn e2e_unknown_tool_reported_and_turn_completes() {
    let e2e = E2E::new(
        vec![
            ScriptedOutput::ToolCalls(vec![("c1".into(), "missing".into())]),
            ScriptedOutput::Text("done".into()),
        ],
        empty_tools(),
    );
    let handle = e2e.create("e2e-unknown-tool").await;
    handle.agent.followup(message("a"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(requests.len(), 2);
    let events = handle.agent.session().events();
    let result = events
        .iter()
        .find(|e| e.event_type == "tool/result")
        .expect("tool/result");
    assert_eq!(
        result
            .data
            .get("error")
            .and_then(|error| error.as_str_at("code")),
        Some("UNKNOWN_TOOL"),
        "an unknown tool normalizes to a structured error result"
    );
    // The error result is passed back to the model in the next request.
    let second = &requests[1].messages;
    assert!(
        second.iter().any(|m| matches!(
            m.content.first(),
            Some(starriver_kernel::llm::ContentBlock::ToolResult { is_error: true, .. })
        )),
        "the unknown-tool error must be part of the next request history"
    );
    assert_eq!(turn_end_kind(&events).as_deref(), Some("completed"));
    handle.dispose();
}

fn tool_def(
    name: &str,
    executor: Arc<dyn starriver_kernel::tools::ToolExecutor>,
) -> starriver_kernel::tools::ToolDefinition {
    starriver_kernel::tools::ToolDefinition {
        requires_approval: false,
        deny_in_readonly: false,
        name: name.into(),
        description: name.into(),
        parameters: cordis_rslite::Value::new(()),
        output: cordis_rslite::Value::new(()),
        render: Arc::new(|_, value| {
            vec![starriver_kernel::llm::ContentBlock::Text(format!(
                "{value:?}"
            ))]
        }),
        executor,
        is_concurrency_safe: None,
        timeout: None,
        finalize_content: None,
    }
}

#[tokio::test]
async fn e2e_tool_body_error_is_structured_and_turn_continues() {
    use starriver_kernel::tools::ToolError;

    struct FailTool;
    impl starriver_kernel::tools::ToolExecutor for FailTool {
        fn execute(
            &self,
            _arguments: cordis_rslite::Value,
            _ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<
            Result<cordis_rslite::Value, starriver_kernel::tools::ToolError>,
        > {
            Box::pin(async {
                Err(ToolError::Execution {
                    tool: "fail".into(),
                    code: "TOOL_BODY".into(),
                    message: "boom".into(),
                })
            })
        }
    }

    let tools_runtime = starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let _h = tools_runtime
        .register(tool_def("fail", Arc::new(FailTool)))
        .unwrap();
    let e2e = E2E::new(
        vec![
            ScriptedOutput::ToolCalls(vec![("c1".into(), "fail".into())]),
            ScriptedOutput::Text("done".into()),
        ],
        Arc::new(tools_runtime) as Arc<dyn ToolRuntimeView>,
    );
    let handle = e2e.create("e2e-tool-error").await;
    handle.agent.followup(message("a"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(requests.len(), 2);
    let events = handle.agent.session().events();
    let result = events
        .iter()
        .find(|e| e.event_type == "tool/result")
        .expect("tool/result");
    assert_eq!(
        result
            .data
            .get("error")
            .and_then(|error| error.as_str_at("code")),
        Some("TOOL_BODY")
    );
    let second = &requests[1].messages;
    assert!(
        second.iter().any(|m| matches!(
            m.content.first(),
            Some(starriver_kernel::llm::ContentBlock::ToolResult { is_error: true, .. })
        )),
        "tool body error must be passed back to the model"
    );
    assert_eq!(turn_end_kind(&events).as_deref(), Some("completed"));
    handle.dispose();
}

#[tokio::test]
async fn e2e_tool_body_panic_is_contained() {
    struct PanicTool;
    impl starriver_kernel::tools::ToolExecutor for PanicTool {
        fn execute(
            &self,
            _arguments: cordis_rslite::Value,
            _ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<
            Result<cordis_rslite::Value, starriver_kernel::tools::ToolError>,
        > {
            Box::pin(async { panic!("tool body panic") })
        }
    }

    let tools_runtime = starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let _h = tools_runtime
        .register(tool_def("panic", Arc::new(PanicTool)))
        .unwrap();
    let e2e = E2E::new(
        vec![
            ScriptedOutput::ToolCalls(vec![("c1".into(), "panic".into())]),
            ScriptedOutput::Text("done".into()),
        ],
        Arc::new(tools_runtime) as Arc<dyn ToolRuntimeView>,
    );
    let handle = e2e.create("e2e-tool-panic").await;
    handle.agent.followup(message("a"));
    idle(&handle.agent).await;

    let events = handle.agent.session().events();
    let result = events
        .iter()
        .find(|e| e.event_type == "tool/result")
        .expect("tool/result");
    assert_eq!(
        result
            .data
            .get("error")
            .and_then(|error| error.as_str_at("code")),
        Some("TOOL_PANIC"),
        "a panicking tool body must be normalized, not escape to the driver"
    );
    assert_eq!(turn_end_kind(&events).as_deref(), Some("completed"));
    handle.dispose();
}

#[tokio::test]
async fn e2e_pre_execute_deny_is_reported() {
    use starriver_kernel::tools::{PreToolDecision, ToolsPreExecute};

    let tools_bus = EventBus::new();
    let tools_ctx = Context::new().with_event_bus(&tools_bus);
    let tools_runtime = starriver_tools::InMemoryToolRuntime::new(tools_bus, tools_ctx.clone());
    let _h = tools_runtime
        .register(tool_def("echo", Arc::new(Echo)))
        .unwrap();
    let _deny = tools_ctx.on_waterfall::<ToolsPreExecute, _>(|_, _next| {
        Box::pin(async {
            Ok(PreToolDecision::Deny {
                reason: "blocked by policy".into(),
            })
        })
    });
    let e2e = E2E::new(
        vec![
            ScriptedOutput::ToolCalls(vec![("c1".into(), "echo".into())]),
            ScriptedOutput::Text("done".into()),
        ],
        Arc::new(tools_runtime) as Arc<dyn ToolRuntimeView>,
    );
    let handle = e2e.create("e2e-deny").await;
    handle.agent.followup(message("a"));
    idle(&handle.agent).await;

    let events = handle.agent.session().events();
    let result = events
        .iter()
        .find(|e| e.event_type == "tool/result")
        .expect("tool/result");
    assert_eq!(
        result
            .data
            .get("error")
            .and_then(|error| error.as_str_at("code")),
        Some("DENIED")
    );
    assert_eq!(turn_end_kind(&events).as_deref(), Some("completed"));
    handle.dispose();
}

#[tokio::test]
async fn e2e_unhandled_error_ends_turn_but_loop_survives() {
    use starriver_kernel::agent::AgentErrorEvent;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let e2e = E2E::new(
        vec![
            // AUTH is non-retryable: an unhandled failure of this class is
            // terminal for the turn on the FIRST occurrence. (SERVER would
            // now be retried by the built-in policy — that recovery is
            // covered by e2e_transient_failure_exhausts_retries_and_errors.)
            ScriptedOutput::Error {
                code: "AUTH".into(),
                message: "boom".into(),
            },
            ScriptedOutput::Text("ok".into()),
        ],
        empty_tools(),
    );
    let handle = e2e.create("e2e-unhandled").await;
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
    idle(&handle.agent).await;
    let events = handle.agent.session().events();
    assert_eq!(
        turn_end_kind(&events).as_deref(),
        Some("error"),
        "an unhandled stream error is terminal for the turn"
    );
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(errors.load(Ordering::SeqCst), 1, "agent/error must fire");

    // The loop survives: a fresh followup completes normally.
    handle.agent.followup(message("b"));
    idle(&handle.agent).await;
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
async fn e2e_no_route_turn_error_but_agent_alive() {
    use starriver_kernel::agent::AgentErrorEvent;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let e2e = E2E::new(vec![ScriptedOutput::Text("never".into())], empty_tools());
    let handle = e2e
        .create_with("e2e-no-route", AgentOptions::default())
        .await;
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
    idle(&handle.agent).await;
    let events = handle.agent.session().events();
    assert_eq!(
        turn_end_kind(&events).as_deref(),
        Some("error"),
        "missing provider/model must produce a turn error"
    );
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(errors.load(Ordering::SeqCst), 1);
    assert!(
        e2e.requests().is_empty(),
        "no model call may happen without a route"
    );
    assert_eq!(
        handle.agent.status(),
        starriver_kernel::agent::AgentStatus::Idle,
        "the driver must stay alive"
    );
    handle.dispose();
}

#[tokio::test]
async fn e2e_conclude_turn_ends_without_another_call() {
    struct ConcludeTool;
    impl starriver_kernel::tools::ToolExecutor for ConcludeTool {
        fn execute(
            &self,
            arguments: cordis_rslite::Value,
            ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<
            Result<cordis_rslite::Value, starriver_kernel::tools::ToolError>,
        > {
            Box::pin(async move {
                ctx.conclude_turn();
                Ok(arguments)
            })
        }
    }

    let tools_runtime = starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let _h = tools_runtime
        .register(tool_def("conclude", Arc::new(ConcludeTool)))
        .unwrap();
    let e2e = E2E::new(
        vec![ScriptedOutput::ToolCalls(vec![(
            "c1".into(),
            "conclude".into(),
        )])],
        Arc::new(tools_runtime) as Arc<dyn ToolRuntimeView>,
    );
    let handle = e2e.create("e2e-conclude").await;
    handle.agent.followup(message("a"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(requests.len(), 1, "concludeTurn must end the turn");
    let events = handle.agent.session().events();
    assert_eq!(turn_end_kind(&events).as_deref(), Some("completed"));
    handle.dispose();
}

#[tokio::test]
async fn e2e_deferred_context_enters_next_step() {
    struct DeferTool;
    impl starriver_kernel::tools::ToolExecutor for DeferTool {
        fn execute(
            &self,
            _arguments: cordis_rslite::Value,
            ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<
            Result<cordis_rslite::Value, starriver_kernel::tools::ToolError>,
        > {
            Box::pin(async move {
                ctx.defer_context(Message {
                    id: starriver_kernel::llm::MessageId("deferred".into()),
                    role: starriver_kernel::llm::MessageRole::User,
                    content: vec![starriver_kernel::llm::ContentBlock::Text(
                        "deferred context".into(),
                    )],
                    source: starriver_kernel::llm::MessageSource::Plugin {
                        plugin: "e2e".into(),
                        form: None,
                    },
                });
                Ok(cordis_rslite::Value::new(()))
            })
        }
    }

    let tools_runtime = starriver_tools::InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let _h = tools_runtime
        .register(tool_def("defer", Arc::new(DeferTool)))
        .unwrap();
    let e2e = E2E::new(
        vec![
            ScriptedOutput::ToolCalls(vec![("c1".into(), "defer".into())]),
            ScriptedOutput::Text("done".into()),
        ],
        Arc::new(tools_runtime) as Arc<dyn ToolRuntimeView>,
    );
    let handle = e2e.create("e2e-defer").await;
    handle.agent.followup(message("a"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(requests.len(), 2);
    let second = &requests[1].messages;
    assert!(
        message_texts(second)
            .iter()
            .any(|(role, text)| role == "User" && text == "deferred context"),
        "deferred context must be claimed by the next step"
    );
    handle.dispose();
}

#[tokio::test]
async fn e2e_steer_alone_starts_turn() {
    let e2e = E2E::new(vec![ScriptedOutput::Text("ok".into())], empty_tools());
    let handle = e2e.create("e2e-steer-alone").await;
    handle.agent.steer(message("solo"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(requests.len(), 1, "steer must wake an idle driver");
    assert_eq!(
        message_texts(&requests[0].messages),
        vec![("User".into(), "solo".into())],
        "the steer message becomes the turn's first input"
    );
    handle.dispose();
}

#[tokio::test]
async fn e2e_inject_waits_for_followup() {
    let e2e = E2E::new(vec![ScriptedOutput::Text("ok".into())], empty_tools());
    let handle = e2e.create("e2e-inject").await;
    handle.agent.inject(message("ctx-msg"));
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(
        e2e.requests().is_empty(),
        "inject must not wake an idle driver"
    );
    handle.agent.followup(message("prompt"));
    idle(&handle.agent).await;

    let requests = e2e.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        message_texts(&requests[0].messages),
        vec![
            ("User".into(), "ctx-msg".into()),
            ("User".into(), "prompt".into()),
        ],
        "a turn boundary claims next-step input before one queued prompt"
    );
    handle.dispose();
}

#[tokio::test]
async fn e2e_session_created_veto_rolls_back() {
    use cordis_rslite::EventError;
    use starriver_kernel::session::SessionCreated;

    let e2e = E2E::new(vec![ScriptedOutput::Text("never".into())], empty_tools());
    let _veto = e2e
        .bus
        .on_sync::<SessionCreated, _>(|_| Err(EventError::Listener("veto".into())));

    let result = e2e
        .registry
        .create(CreateAgentOptions {
            session_id: SessionId("e2e-veto".into()),
            seed: Vec::new(),
            cwd: None,
            agent_options: AgentOptions::default(),
    agent_preset: None,
        })
        .await;
    assert!(
        result.is_err(),
        "session/created veto must fail publication"
    );
    assert!(
        e2e.registry.get(&SessionId("e2e-veto".into())).is_none(),
        "agent entry must be rolled back"
    );
    assert!(
        e2e.sessions.get(&SessionId("e2e-veto".into())).is_none(),
        "session entry must be rolled back"
    );
}

#[tokio::test]
async fn e2e_duplicate_session_id_create_fails() {
    let e2e = E2E::new(vec![ScriptedOutput::Text("ok".into())], empty_tools());
    let first = e2e.create("e2e-dup").await;

    let result = e2e
        .registry
        .create(CreateAgentOptions {
            session_id: SessionId("e2e-dup".into()),
            seed: Vec::new(),
            cwd: None,
            agent_options: AgentOptions::default(),
    agent_preset: None,
        })
        .await;
    assert!(
        result.is_err(),
        "a second agent on the same session id must be rejected"
    );

    // The original agent is untouched and still functional.
    first.agent.followup(message("a"));
    idle(&first.agent).await;
    let events = first.agent.session().events();
    assert_eq!(turn_end_kind(&events).as_deref(), Some("completed"));
    first.dispose();
}

#[tokio::test]
async fn e2e_session_disposed_paired_on_dispose() {
    use starriver_kernel::session::SessionDisposed;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let e2e = E2E::new(vec![ScriptedOutput::Text("ok".into())], empty_tools());
    let disposed = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&disposed);
    let _listener = e2e.bus.on::<SessionDisposed, _>(move |_| {
        let counter = Arc::clone(&counter);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(cordis_rslite::Value::new(()))
        })
    });

    let handle = e2e.create("e2e-disposed").await;
    assert!(e2e
        .sessions
        .get(&SessionId("e2e-disposed".into()))
        .is_some());
    handle.dispose();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        disposed.load(Ordering::SeqCst),
        1,
        "handle disposal must pair session/disposed"
    );
    assert!(e2e
        .sessions
        .get(&SessionId("e2e-disposed".into()))
        .is_none());
}

#[tokio::test]
async fn e2e_concurrent_create_same_id_one_wins() {
    let e2e = E2E::new(vec![ScriptedOutput::Text("ok".into())], empty_tools());
    let options = || CreateAgentOptions {
        session_id: SessionId("e2e-race".into()),
        seed: Vec::new(),
        cwd: None,
        agent_options: AgentOptions::default(),
    agent_preset: None,
        };
    let (left, right) = tokio::join!(
        e2e.registry.create(options()),
        e2e.registry.create(options()),
    );
    let results = [left, right];
    let wins = results.iter().filter(|result| result.is_ok()).count();
    let losses = results.iter().filter(|result| result.is_err()).count();
    assert_eq!(wins, 1, "exactly one concurrent create must win");
    assert_eq!(losses, 1, "the loser must roll back cleanly");
    assert!(e2e.registry.get(&SessionId("e2e-race".into())).is_some());
    assert!(e2e.sessions.get(&SessionId("e2e-race".into())).is_some());
}

struct FailingSessionStore;

impl SessionStoreView for FailingSessionStore {
    fn prepare(
        &self,
        _id: SessionId,
        _options: starriver_kernel::session::PrepareOptions,
    ) -> Result<
        starriver_kernel::session::SessionPreparation,
        starriver_kernel::session::SessionError,
    > {
        Err(starriver_kernel::session::SessionError::NotFound)
    }
    fn enter(
        &self,
        _session: starriver_kernel::session::Session,
    ) -> Result<starriver_kernel::session::EnterHandle, starriver_kernel::session::SessionError>
    {
        unreachable!("prepare fails first")
    }
    fn announce(
        &self,
        _session: &starriver_kernel::session::Session,
    ) -> Result<(), starriver_kernel::session::SessionError> {
        unreachable!("prepare fails first")
    }
    fn get(&self, _id: &SessionId) -> Option<starriver_kernel::session::Session> {
        None
    }
    fn remove(&self, _id: &SessionId) -> Option<starriver_kernel::session::Session> {
        None
    }
}

#[tokio::test]
async fn e2e_store_prepare_failure_is_clean() {
    let e2e = E2E::with_sessions(
        vec![ScriptedOutput::Text("never".into())],
        empty_tools(),
        Arc::new(FailingSessionStore) as Arc<dyn SessionStoreView>,
    );
    let result = e2e
        .registry
        .create(CreateAgentOptions {
            session_id: SessionId("e2e-store".into()),
            seed: Vec::new(),
            cwd: None,
            agent_options: AgentOptions::default(),
    agent_preset: None,
        })
        .await;
    assert!(result.is_err(), "store failure must fail publication");
    assert!(
        e2e.registry.get(&SessionId("e2e-store".into())).is_none(),
        "no partial agent entry after a store failure"
    );
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
