//! End-to-end over the plugin kernel with a scripted mock model: exercise
//! create -> UiEvent stream (sessionStarted -> assistantText -> turnEnd) and
//! resume continuation.

pub use std::sync::Arc;
use std::sync::Mutex;

use cordis_rslite::{Context, EventBus};
use starriver_host::plugin_kernel::HostSessionMeta;
use starriver_host::plugin_kernel::PluginKernel;
use starriver_kernel::llm::{
    ContentBlock, FinishReason, GenerateOptions, LlmAdapter, LlmError, StreamChunk,
};
use starriver_kernel::ui::UiEvent;
use starriver_session_persistence::SqliteSessionPersistence;

/// Scripted adapter that also records each request's tool names.
fn scripted_adapter(texts: Vec<String>) -> (Arc<dyn LlmAdapter>, Arc<Mutex<Vec<Vec<String>>>>) {
    struct Scripted {
        texts: std::sync::Mutex<std::collections::VecDeque<String>>,
        tools: Arc<Mutex<Vec<Vec<String>>>>,
    }
    impl LlmAdapter for Scripted {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["mock".to_string()])
        }
        fn stream(
            &self,
            options: GenerateOptions,
        ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
            let tools = options
                .tools
                .as_ref()
                .map(|tools| tools.iter().map(|t| t.name.clone()).collect())
                .unwrap_or_default();
            self.tools.lock().unwrap().push(tools);
            let text = self
                .texts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "fallback".into());
            let block_text = text.clone();
            Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::TextDelta { index: 0, text }),
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::Text(block_text),
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                }),
            ]))
        }
    }
    let tools = Arc::new(Mutex::new(Vec::new()));
    let adapter = Arc::new(Scripted {
        texts: std::sync::Mutex::new(texts.into()),
        tools: Arc::clone(&tools),
    }) as Arc<dyn LlmAdapter>;
    (adapter, tools)
}

fn kernel_with_adapter(
    path: std::path::PathBuf,
    adapter: Option<Arc<dyn LlmAdapter>>,
) -> (PluginKernel, EventBus) {
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = SqliteSessionPersistence::open_with_bus(path, bus.clone()).unwrap();
    let kernel = PluginKernel::new(ctx, bus.clone(), Arc::new(persistence));
    if let Some(adapter) = adapter {
        kernel.set_llm_adapter(adapter);
    }
    (kernel, bus)
}

fn kernel_with_script(
    path: std::path::PathBuf,
    texts: Vec<String>,
) -> (PluginKernel, Arc<Mutex<Vec<Vec<String>>>>) {
    let (adapter, tools) = scripted_adapter(texts);
    let (kernel, _bus) = kernel_with_adapter(path, Some(adapter));
    (kernel, tools)
}

async fn drain_until_terminal(
    mut rx: tokio::sync::broadcast::Receiver<serde_json::Value>,
) -> Vec<UiEvent> {
    let mut seen = Vec::new();
    while let Ok(event) = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
    {
        let event: UiEvent = serde_json::from_value(event).unwrap();
        let terminal = matches!(
            event,
            UiEvent::TurnEnd { .. } | UiEvent::TurnError { .. } | UiEvent::Cancelled { .. }
        );
        seen.push(event);
        if terminal {
            break;
        }
    }
    seen
}

/// Adapter with a queue of prebuilt streams, so a test can script the first
/// LLM call and hold the second one on an external channel.
struct MixedAdapter {
    streams: std::sync::Mutex<
        std::collections::VecDeque<
            futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>>,
        >,
    >,
}

impl MixedAdapter {
    fn push(&self, stream: futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>>) {
        self.streams.lock().unwrap().push_back(stream);
    }
}

impl LlmAdapter for MixedAdapter {
    fn providers(&self) -> &[String] {
        static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        PROVIDERS.get_or_init(|| vec!["mock".to_string()])
    }

    fn stream(
        &self,
        _options: GenerateOptions,
    ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
        self.streams
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Box::pin(futures::stream::empty()))
    }
}

async fn wait_for_tool_result(mut rx: tokio::sync::broadcast::Receiver<serde_json::Value>) -> bool {
    while let Ok(event) = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
    {
        let event: UiEvent = serde_json::from_value(event).unwrap();
        if matches!(event, UiEvent::ToolResult { ok: true, .. }) {
            return true;
        }
        if matches!(
            event,
            UiEvent::TurnEnd { .. } | UiEvent::TurnError { .. } | UiEvent::Cancelled { .. }
        ) {
            return false;
        }
    }
    false
}

#[tokio::test]
async fn execute_emits_full_ui_event_flow() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, _tools) = kernel_with_script(dir.path().join("s.db"), vec!["hello".into()]);
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let execution = kernel
        .execute(
            "hi".into(),
            None,
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let seen = drain_until_terminal(rx).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::SessionStarted { .. })));
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::AssistantText { text, .. } if text == "hello")));
    assert!(seen.iter().any(|e| matches!(e, UiEvent::TurnEnd { .. })));
    execution.dispose();
}

#[tokio::test]
async fn execute_resumes_same_session_on_second_call() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, _tools) = kernel_with_script(
        dir.path().join("s.db"),
        vec!["first".into(), "second".into()],
    );
    let id = uuid::Uuid::new_v4().to_string();
    let (tx1, _rx1) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx1 = tx1.subscribe();
    let execution1 = kernel
        .execute(
            "q1".into(),
            Some(id.clone()),
            Some("m1".into()),
            tx1,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    drain_until_terminal(rx1).await;
    execution1.dispose();
    // Dispose detaches asynchronously; give the agent lifecycle time to
    // release the session id before the resume below.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let (tx2, _rx2) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx2 = tx2.subscribe();
    let execution2 = kernel
        .execute(
            "q2".into(),
            Some(id),
            Some("m1".into()),
            tx2,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let seen = drain_until_terminal(rx2).await;
    let event_types: Vec<String> = execution2
        .agent()
        .session()
        .events()
        .iter()
        .map(|e| e.event_type.clone())
        .collect();
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::AssistantText { text, .. } if text == "second")));
    execution2.dispose();
}

#[tokio::test]
async fn agent_request_only_contains_whitelisted_tools() {
    use cordis_rslite::Value;
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::AgentScopeRegistry;
    use starriver_kernel::llm::ContentBlock;
    use starriver_kernel::tools::{ToolDefinition, ToolExecutor, ToolRuntime};
    use std::sync::Arc;

    struct Echo;
    impl ToolExecutor for Echo {
        fn execute(
            &self,
            arguments: Value,
            _ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<Value, starriver_kernel::tools::ToolError>> {
            Box::pin(async { Ok(arguments) })
        }
    }
    fn definition(name: &str) -> ToolDefinition {
        ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: name.into(),
            description: "echo".into(),
            parameters: Value::new(()),
            output: Value::new(()),
            render: Arc::new(|_, value| vec![ContentBlock::Text(format!("{value:?}"))]),
            executor: Arc::new(Echo),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("s.db"), bus.clone()).unwrap(),
    );
    let (adapter, recorded) = scripted_adapter(vec!["hi".into()]);
    let scopes = Arc::new(AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(
        ctx,
        bus.clone(),
        Arc::clone(&persistence),
        Arc::clone(&scopes),
    );
    kernel.set_llm_adapter(adapter);
    // The kernel registers bash/read/write/edit/git/grep plus the batch-2 set
    // (glob/web_fetch/web_search/lsp/patch), the state/execution tools and
    // terminal; add one extra registration. Register-equals-visible: the
    // model request must carry EVERY registered tool.
    let _echo = kernel.tools().register(definition("echo")).unwrap();
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let execution = kernel
        .execute(
            "hi".into(),
            None,
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    drain_until_terminal(rx).await;
    let recorded = recorded.lock().unwrap();
    let request_tools = recorded.last().cloned().unwrap_or_default();
    for visible in [
        "bash",
        "read",
        "write",
        "edit",
        "git",
        "grep",
        "glob",
        "web_fetch",
        "web_search",
        "lsp",
        "patch",
        "todo_write",
        "goal_write",
        "plan_write",
        "python_exec",
        "e2b_run",
        "terminal_start",
        "echo",
    ] {
        assert!(
            request_tools.iter().any(|name| name == visible),
            "registered tool {visible} must be sent to the model"
        );
    }
    // run_code stays hidden in native (non-PTC) presentation.
    assert!(
        !request_tools.iter().any(|name| name == "run_code"),
        "reserved PTC transport must not be sent in native mode"
    );
    execution.dispose();
}

/// Adapter that first emits one tool call, then a closing text.
fn tool_call_adapter(
    tool_name: &'static str,
) -> (Arc<dyn LlmAdapter>, Arc<Mutex<Vec<Vec<String>>>>) {
    tool_call_adapter_args(tool_name, "{}")
}

/// Adapter that first emits one tool call with the given arguments JSON, then
/// a closing text.
fn tool_call_adapter_args(
    tool_name: &'static str,
    args_json: &'static str,
) -> (Arc<dyn LlmAdapter>, Arc<Mutex<Vec<Vec<String>>>>) {
    struct ScriptedToolCall {
        tool_name: &'static str,
        args_json: &'static str,
        calls: std::sync::atomic::AtomicUsize,
        tools: Arc<Mutex<Vec<Vec<String>>>>,
    }
    impl LlmAdapter for ScriptedToolCall {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["mock".to_string()])
        }
        fn stream(
            &self,
            options: GenerateOptions,
        ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
            let tools = options
                .tools
                .as_ref()
                .map(|tools| tools.iter().map(|t| t.name.clone()).collect())
                .unwrap_or_default();
            self.tools.lock().unwrap().push(tools);
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let name = self.tool_name;
            let args_json = self.args_json;
            if call == 0 {
                Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::ToolCallDelta {
                        index: 0,
                        id: starriver_kernel::llm::CallId("c1".into()),
                        name: Some(name.into()),
                        arguments_delta: args_json.into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::ToolCall {
                            id: starriver_kernel::llm::CallId("c1".into()),
                            name: Some(name.into()),
                            arguments: starriver_kernel::json::JsonValue::from_json_str(&args_json)
                                .unwrap(),
                        },
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::ToolCalls,
                        replay_state: None,
                    }),
                ]))
            } else {
                Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::TextDelta {
                        index: 0,
                        text: "done".into(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::Text("done".into()),
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::Stop,
                        replay_state: None,
                    }),
                ]))
            }
        }
    }
    let tools = Arc::new(Mutex::new(Vec::new()));
    let adapter = Arc::new(ScriptedToolCall {
        tool_name,
        args_json,
        calls: std::sync::atomic::AtomicUsize::new(0),
        tools: Arc::clone(&tools),
    }) as Arc<dyn LlmAdapter>;
    (adapter, tools)
}

#[tokio::test]
async fn build_mode_requires_approval_tool_asks_then_allows() {
    use cordis_rslite::Value;
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::AgentScopeRegistry;
    use starriver_kernel::tools::{ToolDefinition, ToolExecutor, ToolRuntime};
    use std::sync::Arc;

    struct Echo;
    impl ToolExecutor for Echo {
        fn execute(
            &self,
            arguments: Value,
            _ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<Value, starriver_kernel::tools::ToolError>> {
            Box::pin(async { Ok(arguments) })
        }
    }
    fn definition(name: &str) -> ToolDefinition {
        ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: name.into(),
            description: "echo".into(),
            parameters: Value::new(()),
            output: Value::new(()),
            render: Arc::new(|_, value| vec![ContentBlock::Text(format!("{value:?}"))]),
            executor: Arc::new(Echo),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let (adapter, _tools) = tool_call_adapter("needs-ok");
    let (kernel, _bus) = kernel_with_adapter(dir.path().join("s.db"), Some(adapter));
    let mut def = definition("needs-ok");
    def.requires_approval = true;
    kernel.tools().register(def).unwrap();
    let kernel = Arc::new(kernel);

    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let sid = "s1".to_string();
    let kernel_approver = Arc::clone(&kernel);
    let approver = tokio::spawn(async move {
        // Poll until the bridge holds the pending request, then allow once.
        for _ in 0..500 {
            if kernel_approver.approval_bridge.approve("s1", true, false) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("approval request never reached the bridge");
    });
    let execution = kernel
        .execute(
            "run".into(),
            Some(sid),
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    approver.await.unwrap();
    let seen = drain_until_terminal(rx).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: true, .. })));
    assert!(seen.iter().any(|e| matches!(e, UiEvent::TurnEnd { .. })));
    execution.dispose();
}

#[tokio::test]
async fn requires_approval_tool_asks_by_default_preset() {
    use cordis_rslite::Value;
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::AgentScopeRegistry;
    use starriver_kernel::tools::{ToolDefinition, ToolExecutor, ToolRuntime};
    use std::sync::Arc;

    struct Echo;
    impl ToolExecutor for Echo {
        fn execute(
            &self,
            arguments: Value,
            _ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<Value, starriver_kernel::tools::ToolError>> {
            Box::pin(async { Ok(arguments) })
        }
    }
    fn definition(name: &str) -> ToolDefinition {
        ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: name.into(),
            description: "echo".into(),
            parameters: Value::new(()),
            output: Value::new(()),
            render: Arc::new(|_, value| vec![ContentBlock::Text(format!("{value:?}"))]),
            executor: Arc::new(Echo),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let (adapter, _tools) = tool_call_adapter("needs-ok");
    let (kernel, _bus) = kernel_with_adapter(dir.path().join("s.db"), Some(adapter));
    let mut def = definition("needs-ok");
    def.requires_approval = true;
    kernel.tools().register(def).unwrap();
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let kernel = Arc::new(kernel);
    let kernel_approver = Arc::clone(&kernel);
    let approver = tokio::spawn(async move {
        for _ in 0..500 {
            if kernel_approver.approval_bridge.approve("s1", true, false) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("default-preset approval request never reached the bridge");
    });
    let execution = kernel
        .execute(
            "run".into(),
            Some("s1".into()),
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    approver.await.unwrap();
    let seen = drain_until_terminal(rx).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: true, .. })));
    // Permission-preset model: requires_approval tools always ask (the
    // default preset is workspace-write + ask).
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ApprovalRequested { .. })));
    execution.dispose();
}

#[tokio::test]
async fn auto_mode_requires_approval_tool_asks_then_allows() {
    use cordis_rslite::Value;
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::AgentScopeRegistry;
    use starriver_kernel::tools::{ToolDefinition, ToolExecutor, ToolRuntime};
    use std::sync::Arc;

    struct Echo;
    impl ToolExecutor for Echo {
        fn execute(
            &self,
            arguments: Value,
            _ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<Value, starriver_kernel::tools::ToolError>> {
            Box::pin(async { Ok(arguments) })
        }
    }
    fn definition(name: &str) -> ToolDefinition {
        ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: name.into(),
            description: "echo".into(),
            parameters: Value::new(()),
            output: Value::new(()),
            render: Arc::new(|_, value| vec![ContentBlock::Text(format!("{value:?}"))]),
            executor: Arc::new(Echo),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let (adapter, _tools) = tool_call_adapter("needs-ok");
    let (kernel, _bus) = kernel_with_adapter(dir.path().join("s.db"), Some(adapter));
    let mut def = definition("needs-ok");
    def.requires_approval = true;
    kernel.tools().register(def).unwrap();
    let kernel = Arc::new(kernel);
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let kernel_approver = Arc::clone(&kernel);
    let approver = tokio::spawn(async move {
        for _ in 0..500 {
            if kernel_approver.approval_bridge.approve("s1", true, false) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("auto-mode approval request never reached the bridge");
    });
    let execution = kernel
        .execute(
            "run".into(),
            Some("s1".into()),
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    approver.await.unwrap();
    let seen = drain_until_terminal(rx).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: true, .. })));
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ApprovalRequested { .. })));
    execution.dispose();
}

#[tokio::test]
async fn respond_approval_with_remember_persists_rule_event() {
    use cordis_rslite::Value;
    use starriver_host::approval_bridge::ApprovalDecision;
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::{AgentRegistry, AgentScopeRegistry};
    use starriver_kernel::tools::{ToolDefinition, ToolExecutor, ToolRuntime};
    use std::sync::Arc;

    struct Echo;
    impl ToolExecutor for Echo {
        fn execute(
            &self,
            arguments: Value,
            _ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<Value, starriver_kernel::tools::ToolError>> {
            Box::pin(async { Ok(arguments) })
        }
    }
    fn definition(name: &str) -> ToolDefinition {
        ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: name.into(),
            description: "echo".into(),
            parameters: Value::new(()),
            output: Value::new(()),
            render: Arc::new(|_, value| vec![ContentBlock::Text(format!("{value:?}"))]),
            executor: Arc::new(Echo),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let (adapter, _tools) = tool_call_adapter("needs-ok");
    let (kernel, _bus) = kernel_with_adapter(dir.path().join("s.db"), Some(adapter));
    let mut def = definition("needs-ok");
    def.requires_approval = true;
    kernel.tools().register(def).unwrap();
    let kernel = Arc::new(kernel);

    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let kernel_approver = Arc::clone(&kernel);
    let approver = tokio::spawn(async move {
        for _ in 0..500 {
            let events = kernel_approver
                .registry()
                .get(&starriver_kernel::session::SessionId("s1".to_string()))
                .map(|agent| agent.session().events())
                .unwrap_or_default();
            let asked_id = events
                .iter()
                .rev()
                .find(|e| e.event_type == "approval/asked")
                .and_then(|e| e.data.as_str_at("id"))
                .map(String::from);
            if let Some(id) = asked_id {
                kernel_approver
                    .approval_bridge
                    .respond(&id, ApprovalDecision::Allow { remember: true });
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("approval asked event never appeared");
    });
    let execution = kernel
        .execute(
            "run".into(),
            Some("s1".into()),
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    approver.await.unwrap();
    let seen = drain_until_terminal(rx).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: true, .. })));
    let events = execution.agent().session().events();
    assert!(events.iter().any(|e| e.event_type == "approval/rule"));
    let decided = events
        .iter()
        .find(|e| e.event_type == "approval/decided")
        .and_then(|e| e.data.as_str_at("outcome"));
    assert_eq!(decided, Some("allowed-similar"));
    execution.dispose();
}

#[tokio::test]
async fn remembered_rule_survives_resume_without_reasking() {
    use cordis_rslite::Value;
    use starriver_host::approval_bridge::ApprovalDecision;
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::{AgentRegistry, AgentScopeRegistry};
    use starriver_kernel::tools::{ToolDefinition, ToolExecutor, ToolRuntime};
    use std::sync::Arc;

    struct Echo;
    impl ToolExecutor for Echo {
        fn execute(
            &self,
            arguments: Value,
            _ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<Value, starriver_kernel::tools::ToolError>> {
            Box::pin(async { Ok(arguments) })
        }
    }
    fn definition(name: &str) -> ToolDefinition {
        ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: name.into(),
            description: "echo".into(),
            parameters: Value::new(()),
            output: Value::new(()),
            render: Arc::new(|_, value| vec![ContentBlock::Text(format!("{value:?}"))]),
            executor: Arc::new(Echo),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("resume.db");

    // First kernel: ask + remember → approval/rule persisted.
    let (adapter1, _t1) = tool_call_adapter("needs-ok");
    let (kernel1, _b1) = kernel_with_adapter(db.clone(), Some(adapter1));
    let mut def = definition("needs-ok");
    def.requires_approval = true;
    kernel1.tools().register(def).unwrap();
    let kernel1 = Arc::new(kernel1);
    let (tx1, _rx1) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx1 = tx1.subscribe();
    let approver = {
        let kernel = Arc::clone(&kernel1);
        tokio::spawn(async move {
            for _ in 0..500 {
                let asked_id = kernel
                    .registry()
                    .get(&starriver_kernel::session::SessionId("s1".to_string()))
                    .map(|agent| agent.session().events())
                    .unwrap_or_default()
                    .iter()
                    .rev()
                    .find(|e| e.event_type == "approval/asked")
                    .and_then(|e| e.data.as_str_at("id"))
                    .map(String::from);
                if let Some(id) = asked_id {
                    kernel
                        .approval_bridge
                        .respond(&id, ApprovalDecision::Allow { remember: true });
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("first approval asked event never appeared");
        })
    };
    let execution1 = kernel1
        .execute(
            "run".into(),
            Some("s1".into()),
            Some("m1".into()),
            tx1,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    approver.await.unwrap();
    drain_until_terminal(rx1).await;
    assert!(execution1
        .agent()
        .session()
        .events()
        .iter()
        .any(|e| e.event_type == "approval/rule"));
    execution1.dispose();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Second kernel on the same db: resume reuses the remembered rule, so the
    // same tool runs without a new approval question.
    let (adapter2, _t2) = tool_call_adapter("needs-ok");
    let (kernel2, _b2) = kernel_with_adapter(db, Some(adapter2));
    let mut def2 = definition("needs-ok");
    def2.requires_approval = true;
    kernel2.tools().register(def2).unwrap();
    let (tx2, _rx2) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx2 = tx2.subscribe();
    let execution2 = kernel2
        .execute(
            "run again".into(),
            Some("s1".into()),
            Some("m1".into()),
            tx2,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let seen = drain_until_terminal(rx2).await;
    let event_types: Vec<String> = execution2
        .agent()
        .session()
        .events()
        .iter()
        .map(|e| e.event_type.clone())
        .collect();
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: true, .. })));
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, UiEvent::ApprovalRequested { .. })),
        "resume re-asked; session event types: {event_types:?}"
    );
    execution2.dispose();
}

#[tokio::test]
async fn cancel_during_pending_approval_resolves_cancelled() {
    use cordis_rslite::Value;
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::{AgentCancelCause, AgentCancelKind, AgentScopeRegistry};
    use starriver_kernel::tools::{ToolDefinition, ToolExecutor, ToolRuntime};
    use std::sync::Arc;

    struct Echo;
    impl ToolExecutor for Echo {
        fn execute(
            &self,
            arguments: Value,
            _ctx: starriver_kernel::tools::ToolRunContext,
        ) -> cordis_rslite::BoxFuture<Result<Value, starriver_kernel::tools::ToolError>> {
            Box::pin(async { Ok(arguments) })
        }
    }
    fn definition(name: &str) -> ToolDefinition {
        ToolDefinition {
            requires_approval: false,
            deny_in_readonly: false,
            name: name.into(),
            description: "echo".into(),
            parameters: Value::new(()),
            output: Value::new(()),
            render: Arc::new(|_, value| vec![ContentBlock::Text(format!("{value:?}"))]),
            executor: Arc::new(Echo),
            is_concurrency_safe: None,
            timeout: None,
            finalize_content: None,
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let (adapter, _tools) = tool_call_adapter("needs-ok");
    let (kernel, _bus) = kernel_with_adapter(dir.path().join("s.db"), Some(adapter));
    let mut def = definition("needs-ok");
    def.requires_approval = true;
    kernel.tools().register(def).unwrap();
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let execution = kernel
        .execute(
            "run".into(),
            Some("s1".into()),
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    // Let the pending approval question reach the bridge (no answerer is
    // configured in this test), then cancel the turn.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    execution
        .agent()
        .cancel(AgentCancelCause::new(AgentCancelKind::User), false);
    let seen = drain_until_terminal(rx).await;
    assert!(seen.iter().any(|e| matches!(e, UiEvent::Cancelled { .. })));
    let events = execution.agent().session().events();
    let decided = events
        .iter()
        .find(|e| e.event_type == "approval/decided")
        .and_then(|e| e.data.as_str_at("outcome"));
    assert_eq!(decided, Some("cancelled"));
    execution.dispose();
}

#[tokio::test]
async fn workspace_write_preset_allows_write_after_approval() {
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::AgentScopeRegistry;
    use starriver_kernel::tools::ToolRuntime;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let (adapter, _t) = tool_call_adapter_args("write", r#"{"path":"out.txt","content":"data"}"#);
    let (kernel, _bus) = kernel_with_adapter(dir.path().join("s.db"), Some(adapter));
    // Default preset: workspace-write + ask.
    let kernel = Arc::new(kernel);
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let approver = {
        let kernel = Arc::clone(&kernel);
        tokio::spawn(async move {
            for _ in 0..500 {
                if kernel.approval_bridge.approve("s-write", true, false) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("write approval never reached the bridge");
        })
    };
    let execution = kernel
        .execute(
            "write a file".into(),
            Some("s-write".into()),
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    approver.await.unwrap();
    let seen = drain_until_terminal(rx).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: true, .. })));
    execution.dispose();
}

#[tokio::test]
async fn read_only_preset_rejects_write() {
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_host::preset::Preset;
    use starriver_kernel::agent::AgentScopeRegistry;
    use starriver_kernel::sandbox::SandboxMode;
    use starriver_kernel::tools::ToolRuntime;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let (adapter, _t) = tool_call_adapter_args("write", r#"{"path":"out.txt","content":"data"}"#);
    let (kernel, _bus) = kernel_with_adapter(dir.path().join("s.db"), Some(adapter));
    kernel.set_preset(Preset {
        sandbox: SandboxMode::ReadOnly,
        approval: starriver_agent_loop::approval::ApprovalMode::Manual,
    });
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let execution = kernel
        .execute(
            "write a file".into(),
            Some("s-ro".into()),
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let seen = drain_until_terminal(rx).await;
    let result = seen
        .iter()
        .find(|e| matches!(e, UiEvent::ToolResult { .. }))
        .unwrap();
    let UiEvent::ToolResult { ok, .. } = result else {
        unreachable!()
    };
    assert!(!ok, "write must fail in read-only sandbox");
    // The durable error carries the read-only denial reason.
    let events = execution.agent().session().events();
    let tool_result = events
        .iter()
        .find(|e| e.event_type == "tool/result")
        .unwrap();
    let message = starriver_kernel::llm::Message::from_json(
        tool_result
            .data
            .get("message")
            .cloned()
            .unwrap_or(starriver_kernel::json::JsonValue::Null),
    )
    .unwrap();
    assert!(
        format!("{message:?}").contains("read-only"),
        "message: {message:?}"
    );
    execution.dispose();
}

#[tokio::test]
async fn danger_full_access_preset_runs_without_asking() {
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_host::preset::Preset;
    use starriver_kernel::agent::AgentScopeRegistry;
    use starriver_kernel::sandbox::SandboxMode;
    use starriver_kernel::tools::ToolRuntime;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let (adapter, _t) = tool_call_adapter_args("write", r#"{"path":"out.txt","content":"data"}"#);
    let (kernel, _bus) = kernel_with_adapter(dir.path().join("s.db"), Some(adapter));
    kernel.set_preset(Preset {
        sandbox: SandboxMode::DangerFullAccess,
        approval: starriver_agent_loop::approval::ApprovalMode::ByPass,
    });
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let execution = kernel
        .execute(
            "write a file".into(),
            Some("s-danger".into()),
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let seen = drain_until_terminal(rx).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: true, .. })));
    assert!(!seen
        .iter()
        .any(|e| matches!(e, UiEvent::ApprovalRequested { .. })));
    execution.dispose();
}

/// Minimal agent double for bridge waterfall tests.
struct BridgeFakeAgent {
    id: starriver_kernel::session::SessionId,
    session: starriver_kernel::session::Session,
    agent_ctx: Context,
}

impl BridgeFakeAgent {
    fn new(session: starriver_kernel::session::Session, bus: &EventBus) -> Arc<Self> {
        Arc::new(Self {
            id: starriver_kernel::session::SessionId("s1".to_string()),
            session,
            agent_ctx: Context::new().with_event_bus(bus),
        })
    }
}

impl starriver_kernel::agent::Agent for BridgeFakeAgent {
    fn id(&self) -> &starriver_kernel::session::SessionId {
        &self.id
    }
    fn options(&self) -> &starriver_kernel::agent::AgentOptions {
        static OPTIONS: std::sync::OnceLock<starriver_kernel::agent::AgentOptions> =
            std::sync::OnceLock::new();
        OPTIONS.get_or_init(starriver_kernel::agent::AgentOptions::default)
    }
    fn session(&self) -> &starriver_kernel::session::Session {
        &self.session
    }
    fn status(&self) -> starriver_kernel::agent::AgentStatus {
        starriver_kernel::agent::AgentStatus::Idle
    }
    fn ctx(&self) -> &Context {
        &self.agent_ctx
    }
    fn followup(&self, _message: starriver_kernel::llm::Message) {}
    fn steer(&self, _message: starriver_kernel::llm::Message) {}
    fn inject(&self, _message: starriver_kernel::llm::Message) {}
    fn send(
        &self,
        _message: starriver_kernel::llm::Message,
        _target: starriver_kernel::agent::InboxTarget,
        _wakeup: bool,
    ) {
    }
    fn cancel(&self, _cause: starriver_kernel::agent::AgentCancelCause, _keep_inbox: bool) -> bool {
        false
    }
    fn when_idle(&self) -> cordis_rslite::BoxFuture<()> {
        Box::pin(async {})
    }
}

#[tokio::test]
async fn approval_bridge_aborted_signal_resolves_cancelled() {
    use starriver_kernel::agent::AgentRegistry;
    use starriver_kernel::approval::{ApprovalOutcome, ApprovalRequest, ApprovalRequestEvent};
    use starriver_kernel::json::JsonValue;

    let bus = EventBus::new();
    let session = starriver_kernel::session::Session::new(
        starriver_kernel::session::SessionId("s1".to_string()),
        bus.clone(),
    );
    session
        .append(
            "approval/asked",
            JsonValue::Object(vec![
                ("id".into(), JsonValue::String("ap-1".into())),
                ("toolName".into(), JsonValue::String("bash".into())),
            ]),
        )
        .unwrap();
    let agent = BridgeFakeAgent::new(session, &bus);
    let agent_ctx = agent.agent_ctx.clone();
    let registry = Arc::new(starriver_agent_loop::registry::InMemoryAgentRegistry::new());
    registry.enter(agent, None).unwrap();
    let bridge = Arc::new(starriver_host::approval_bridge::ApprovalBridge::new(
        Arc::clone(&registry) as Arc<dyn starriver_kernel::agent::AgentRegistry>,
    ));
    let _handle = bridge.register(&bus);
    let signal = tokio_util::sync::CancellationToken::new();
    signal.cancel();
    let outcome = bus
        .waterfall_scoped(
            &agent_ctx,
            Arc::new(ApprovalRequestEvent {
                req: ApprovalRequest {
                    agent_id: "s1".into(),
                    tool_name: "bash".into(),
                    call_id: None,
                    reason: None,
                    signal: Some(signal),
                    arguments: None,
                },
                signal: None,
            }),
            || Box::pin(async { Ok(ApprovalOutcome::Unavailable) }),
        )
        .await
        .unwrap();
    assert_eq!(outcome, ApprovalOutcome::Cancelled);
}

#[tokio::test]
async fn approval_bridge_missing_agent_fails_closed() {
    use starriver_kernel::approval::{ApprovalOutcome, ApprovalRequest, ApprovalRequestEvent};

    let bus = EventBus::new();
    let agent_ctx = Context::new().with_event_bus(&bus);
    let registry = Arc::new(starriver_agent_loop::registry::InMemoryAgentRegistry::new());
    let bridge = Arc::new(starriver_host::approval_bridge::ApprovalBridge::new(
        Arc::clone(&registry) as Arc<dyn starriver_kernel::agent::AgentRegistry>,
    ));
    let _handle = bridge.register(&bus);
    let outcome = bus
        .waterfall_scoped(
            &agent_ctx,
            Arc::new(ApprovalRequestEvent {
                req: ApprovalRequest {
                    agent_id: "missing".into(),
                    tool_name: "bash".into(),
                    call_id: None,
                    reason: None,
                    signal: None,
                    arguments: None,
                },
                signal: None,
            }),
            || Box::pin(async { Ok(ApprovalOutcome::Unavailable) }),
        )
        .await
        .unwrap();
    assert_eq!(outcome, ApprovalOutcome::Unavailable);
}

#[tokio::test]
async fn approval_bridge_without_matching_asked_fails_closed() {
    use starriver_kernel::agent::AgentRegistry;
    use starriver_kernel::approval::{ApprovalOutcome, ApprovalRequest, ApprovalRequestEvent};

    let bus = EventBus::new();
    let session = starriver_kernel::session::Session::new(
        starriver_kernel::session::SessionId("s1".to_string()),
        bus.clone(),
    );
    // No approval/asked event in the log: the bridge must not invent a
    // pending question for this request.
    let agent = BridgeFakeAgent::new(session, &bus);
    let agent_ctx = agent.agent_ctx.clone();
    let registry = Arc::new(starriver_agent_loop::registry::InMemoryAgentRegistry::new());
    registry.enter(agent, None).unwrap();
    let bridge = Arc::new(starriver_host::approval_bridge::ApprovalBridge::new(
        Arc::clone(&registry) as Arc<dyn starriver_kernel::agent::AgentRegistry>,
    ));
    let _handle = bridge.register(&bus);
    let outcome = bus
        .waterfall_scoped(
            &agent_ctx,
            Arc::new(ApprovalRequestEvent {
                req: ApprovalRequest {
                    agent_id: "s1".into(),
                    tool_name: "bash".into(),
                    call_id: None,
                    reason: None,
                    signal: None,
                    arguments: None,
                },
                signal: None,
            }),
            || Box::pin(async { Ok(ApprovalOutcome::Unavailable) }),
        )
        .await
        .unwrap();
    assert_eq!(outcome, ApprovalOutcome::Unavailable);
}

#[tokio::test]
async fn runtime_context_snapshot_appears_in_session_log() {
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::AgentScopeRegistry;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("s.db"), bus.clone()).unwrap(),
    );
    let (adapter, _recorded) = scripted_adapter(vec!["hi".into()]);
    let kernel = PluginKernel::with_scopes(
        ctx,
        bus.clone(),
        Arc::clone(&persistence),
        Arc::new(AgentScopeRegistry::new()),
    );
    kernel.set_llm_adapter(adapter);
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let execution = kernel
        .execute(
            "hi".into(),
            None,
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    drain_until_terminal(rx).await;
    // The global cwd context is registered; the session log must contain a
    // runtime-context snapshot user/message.
    let session_id = execution.agent().id().0.clone();
    use starriver_kernel::persistence::SessionPersistence as _;
    persistence.flush().await.unwrap();
    let inspection = persistence
        .load(
            starriver_kernel::session::SessionId(session_id.clone()),
            None,
        )
        .await
        .unwrap();
    let has_snapshot = inspection.events.iter().any(|event| {
        event.event_type == "user/message"
            && starriver_kernel::llm::Message::from_json(event.data.clone())
                .ok()
                .and_then(|m| match m.source {
                    starriver_kernel::llm::MessageSource::Plugin { plugin, .. } => {
                        Some(plugin == "runtime-context")
                    }
                    _ => None,
                })
                .unwrap_or(false)
    });
    assert!(
        has_snapshot,
        "session log must contain the runtime-context snapshot"
    );
    execution.dispose();
}

#[tokio::test]
async fn readonly_preset_denies_patch_before_approval() {
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::AgentScopeRegistry;
    use starriver_kernel::sandbox::SandboxMode;

    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("s.db"), bus.clone()).unwrap(),
    );
    let (adapter, _t) = tool_call_adapter_args(
        "patch",
        r#"{"path":"/nonexistent/a.txt","diff":"@@ -1 +1 @@\n-x\n+y\n"}"#,
    );
    let scopes = Arc::new(AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(ctx, bus.clone(), Arc::clone(&persistence), scopes);
    kernel.set_preset(starriver_host::preset::Preset {
        sandbox: SandboxMode::ReadOnly,
        approval: starriver_agent_loop::approval::ApprovalMode::Manual,
    });
    kernel.set_llm_adapter(adapter);
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let rx = tx.subscribe();
    let execution = kernel
        .execute(
            "run".into(),
            None,
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let seen = drain_until_terminal(rx).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: false, .. })));
    execution.dispose();
}

#[tokio::test]
async fn cancel_token_aborts_running_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, _bus) = kernel_with_adapter(dir.path().join("s.db"), None);
    let target = dir.path().join("target.txt");
    std::fs::write(&target, "payload").unwrap();

    let mixed = Arc::new(MixedAdapter {
        streams: std::sync::Mutex::new(std::collections::VecDeque::new()),
    });
    let adapter: Arc<dyn LlmAdapter> = mixed.clone();
    // First LLM call: script a `read` tool call so the loop executes a tool and
    // then asks the model again — that second call is the held one.
    let args = serde_json::json!({"path": target.to_string_lossy()}).to_string();
    mixed.push(Box::pin(futures::stream::iter(vec![
        Ok(StreamChunk::ToolCallDelta {
            index: 0,
            id: starriver_kernel::llm::CallId("c1".into()),
            name: Some("read".into()),
            arguments_delta: args.clone(),
        }),
        Ok(StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: starriver_kernel::llm::CallId("c1".into()),
                name: Some("read".into()),
                arguments: starriver_kernel::json::JsonValue::from_json_str(&args).unwrap_or_else(
                    |_| starriver_kernel::json::JsonValue::String(args.to_string()),
                ),
            },
        }),
        Ok(StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
            replay_state: None,
        }),
    ])));
    let (tx_chunk, rx_chunk) = tokio::sync::mpsc::unbounded_channel();
    mixed.push(Box::pin(futures::stream::unfold(
        rx_chunk,
        |mut rx| async move {
            let chunk = rx.recv().await?;
            Some((chunk, rx))
        },
    )));
    kernel.set_llm_adapter(adapter);

    let cancel = tokio_util::sync::CancellationToken::new();
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let rx = tx.subscribe();
    let execution = kernel
        .execute(
            "go".into(),
            None,
            Some("m1".into()),
            tx.clone(),
            cancel.clone(),
        )
        .await
        .unwrap();

    // The tool result proves the first LLM call finished and the second call
    // (held on the channel) is now in flight.
    assert!(
        wait_for_tool_result(rx).await,
        "tool result should arrive before the turn is held"
    );

    cancel.cancel();
    tx_chunk
        .send(Ok(StreamChunk::Finish {
            reason: FinishReason::Stop,
            replay_state: None,
        }))
        .unwrap();
    // Close the held stream so the loop exits its stream loop and observes
    // the abort at the step boundary.
    drop(tx_chunk);

    let mut rx = tx.subscribe();
    let mut terminal = Vec::new();
    while let Ok(event) = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
    {
        let event: UiEvent = serde_json::from_value(event).unwrap();
        let is_terminal = matches!(
            event,
            UiEvent::TurnEnd { .. } | UiEvent::TurnError { .. } | UiEvent::Cancelled { .. }
        );
        terminal.push(event);
        if is_terminal {
            break;
        }
    }
    assert!(
        terminal
            .iter()
            .any(|event| matches!(event, UiEvent::Cancelled { .. })),
        "cancelled turn must surface UiEvent::Cancelled, got {terminal:?}"
    );
    assert!(
        !terminal
            .iter()
            .any(|event| matches!(event, UiEvent::TurnEnd { .. })),
        "an aborted turn must not surface TurnEnd"
    );
    execution.dispose();
}

#[tokio::test]
async fn execute_meta_appends_title_without_surface_visibility() {
    use starriver_kernel::agent::Agent;

    let dir = tempfile::tempdir().unwrap();
    let (kernel, _bus) = kernel_with_script(dir.path().join("s.db"), vec!["hi".into()]);
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let execution = kernel
        .execute_with_meta(
            "hi".into(),
            None,
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
            Some(HostSessionMeta {
                title: Some("my session".into()),
                reject_images: false,
            }),
        )
        .await
        .unwrap();
    let seen = drain_until_terminal(rx).await;
    assert!(seen
        .iter()
        .any(|event| matches!(event, UiEvent::TurnEnd { .. })));

    let session = execution.agent().session();
    let events = session.events();
    let title_event = events
        .iter()
        .find(|event| event.event_type == "session/title")
        .expect("session/title must be appended from execute meta");
    assert!(
        title_event.surface_op.is_none(),
        "host meta events must never carry a surface op"
    );
    let messages = session.derive_messages().unwrap();
    assert!(messages.iter().all(|message| {
        !message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text(text) if text == "my session"))
    }));
    execution.dispose();
}

#[tokio::test]
async fn execute_projects_title_into_host_store_projection() {
    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("s.db"), bus.clone()).unwrap(),
    );
    let kernel = PluginKernel::new(ctx, bus.clone(), persistence);
    let store =
        starriver_host::host_store::HostStore::open(dir.path().join("s.host.db"), bus.clone())
            .unwrap();
    kernel.set_llm_adapter(scripted_adapter(vec!["hi".into()]).0);

    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let execution = kernel
        .execute_with_meta(
            "hello task".into(),
            Some("s-e2e".into()),
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
            Some(HostSessionMeta {
                title: Some("hello task".into()),
                reject_images: false,
            }),
        )
        .await
        .unwrap();
    let seen = drain_until_terminal(rx).await;
    assert!(seen
        .iter()
        .any(|event| matches!(event, UiEvent::TurnEnd { .. })));

    let row = store.get("s-e2e").expect("projection row must exist");
    assert_eq!(row.title.as_deref(), Some("hello task"));
    execution.dispose();
}

// ── SP1b text-equality comparison (identity+persona+skills fixed fixture) ──

/// Adapter that records the outgoing `system` of every request.
struct SystemRecordingAdapter {
    providers: Vec<String>,
    systems: Arc<Mutex<Vec<Option<String>>>>,
}
impl LlmAdapter for SystemRecordingAdapter {
    fn providers(&self) -> &[String] {
        &self.providers
    }
    fn stream(
        &self,
        options: GenerateOptions,
    ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
        self.systems.lock().unwrap().push(options.system.clone());
        let text = "ok".to_string();
        Box::pin(futures::stream::iter(vec![
            Ok(StreamChunk::TextDelta {
                index: 0,
                text: text.clone(),
            }),
            Ok(StreamChunk::BlockEnd {
                index: 0,
                block: ContentBlock::Text(text),
            }),
            Ok(StreamChunk::Finish {
                reason: FinishReason::Stop,
                replay_state: None,
            }),
        ]))
    }
}

const SP1B_PERSONA: &str = "You are a meticulous systems engineer. Prefer clarity over cleverness.";
const SP1B_SKILL_MD: &str = "---\nname: code-style\ndescription: Keep code tidy\nalways-on: true\n---\n\nAlways format with rustfmt.";
/// Byte-exact expectation for the skills resident (legacy inject format,
/// minus its leading separator which the registry join supplies).
const SP1B_SKILLS_BLOCK: &str =
    "## Always-Active Skills\n\n### code-style\n\nAlways format with rustfmt.\n\n---\n\n";

/// One kernel run to terminal, returning the execution for inspection.
async fn run_sp1b(
    kernel: &PluginKernel,
    bus: &EventBus,
    task: &str,
    sid: &str,
) -> starriver_host::plugin_kernel::KernelExecution {
    let (tx, rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let execution = kernel
        .execute_with_meta_in(
            task.to_string(),
            Some(sid.to_string()),
            None,
            tx,
            tokio_util::sync::CancellationToken::new(),
            None,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await
        .unwrap();
    starriver_host::plugin_kernel::wait_terminal(execution.agent().clone(), bus.clone()).await;
    execution
}

#[tokio::test]
async fn sp1b_system_assembly_identity_persona_skills_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("sp1b.db"), bus.clone()).unwrap(),
    );
    let kernel = PluginKernel::with_scopes(
        ctx,
        bus.clone(),
        persistence,
        Arc::new(starriver_kernel::agent::AgentScopeRegistry::new()),
    );

    // Fixture residents: persona body + one always-on skill.
    kernel.set_persona(Some(SP1B_PERSONA.into())).await.unwrap();
    let manager = Arc::new(starriver_skills::profile::skill_manager::SkillManager::new());
    manager.load_skill(SP1B_SKILL_MD).unwrap();
    kernel.attach_skill_manager(Arc::clone(&manager));

    let systems = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    kernel.set_llm_adapter(Arc::new(SystemRecordingAdapter {
        providers: vec!["mock".into()],
        systems: Arc::clone(&systems),
    }));

    let sid = "sp1b-verbatim".to_string();

    // 1. Steady state: identity + persona lead verbatim and the skills block
    // passes through untransformed. The bare kernel also assembles the
    // built-in default residents (language / communication / tool guides)
    // after the persona; those evolve with the prompt layer, so the seam
    // contract is checked structurally rather than byte-for-byte.
    let execution = run_sp1b(&kernel, &bus, "steady", &sid).await;
    let system = systems
        .lock()
        .unwrap()
        .last()
        .cloned()
        .flatten()
        .unwrap_or_default();
    let leads = format!(
        "{}\n\n{}",
        starriver_host::plugin_kernel::IDENTITY_PROMPT,
        SP1B_PERSONA
    );
    assert!(
        system.starts_with(leads.as_str()),
        "identity+persona must pass through the seam untransformed and lead: {system:?}"
    );
    let persona_at = system.find(SP1B_PERSONA).unwrap_or(usize::MAX);
    let skills_at = system.find(SP1B_SKILLS_BLOCK).unwrap_or(usize::MAX);
    assert!(
        system.contains(SP1B_SKILLS_BLOCK) && skills_at > persona_at,
        "skills block must pass through verbatim, after the persona: {system:?}"
    );
    // Runtime-context snapshot face is unchanged by the migration: the cwd
    // contribution still opens the snapshot body (no cwd → legacy marker).
    let events = execution.agent().session().events();
    let snapshot = events
        .iter()
        .find(|e| {
            e.event_type == "user/message"
                && format!("{:?}", e.data).contains("Current runtime context")
        })
        .map(|e| format!("{:?}", e.data))
        .unwrap_or_default();
    eprintln!("DEBUG SNAPSHOT: {:?}", snapshot);
    for e in execution.agent().session().events() {
        let data_str = format!("{:?}", e.data);
        let preview = if data_str.len() > 80 {
            &data_str[..80]
        } else {
            &data_str
        };
        eprintln!(
            "EVENT seq={} type={} op={:?} data={}",
            e.seq,
            e.event_type,
            e.surface_op.map(|op| format!("{op:?}")),
            preview
        );
    }
    assert!(
        snapshot.contains("Current runtime context")
            && snapshot.contains("workspace-write")
            && !snapshot.contains("（无工作目录）"),
        "SP2: snapshot face = policy contributors only (sandbox first-class): {snapshot:?}"
    );
    execution.dispose();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // 2. Review mode replaces the whole system; exit restores the assembly.
    kernel
        .enter_review_mode("**REVIEW MODE**".into(), None)
        .await
        .unwrap();
    let execution = run_sp1b(&kernel, &bus, "review turn", &sid).await;
    assert_eq!(
        systems.lock().unwrap().last().and_then(|s| s.as_deref()),
        Some("**REVIEW MODE**"),
        "complete:true resident must replace every other section"
    );
    execution.dispose();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    kernel.exit_review_mode().await;
    let execution = run_sp1b(&kernel, &bus, "back to work", &sid).await;
    let system = systems
        .lock()
        .unwrap()
        .last()
        .cloned()
        .flatten()
        .unwrap_or_default();
    assert!(
        system.starts_with(leads.as_str()) && system.contains(SP1B_SKILLS_BLOCK),
        "disposing the review resident restores identity+persona+skills: {system:?}"
    );
    execution.dispose();
}

#[tokio::test]
async fn sp1b_default_kernel_identity_leads_without_persona_or_skills() {
    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("sp1b.db"), bus.clone()).unwrap(),
    );
    let kernel = PluginKernel::new(ctx, bus.clone(), persistence);
    let systems = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    kernel.set_llm_adapter(Arc::new(SystemRecordingAdapter {
        providers: vec!["mock".into()],
        systems: Arc::clone(&systems),
    }));
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let execution = run_sp1b(&kernel, &bus, "hello", "sp1b-identity").await;
    drop(tx);
    execution.dispose();
    // The bare kernel assembles identity plus the built-in default residents
    // (language / communication / tool guides). The seam contract: identity
    // still leads verbatim, and empty persona/skills drop out entirely — no
    // separator trace around them.
    let system = systems
        .lock()
        .unwrap()
        .last()
        .cloned()
        .flatten()
        .unwrap_or_default();
    assert!(
        system.starts_with(starriver_host::plugin_kernel::IDENTITY_PROMPT),
        "identity must lead the default assembly verbatim: {system:?}"
    );
    assert!(
        !system.contains("\n\n\n"),
        "empty persona/skills must leave no separator trace: {system:?}"
    );
}

// ── Phase A test hardening: SP2/SP3 error paths and the full chain ──────

fn user_surface_texts(execution: &starriver_host::plugin_kernel::KernelExecution) -> Vec<String> {
    let events = execution.agent().session().events();
    events
        .iter()
        .filter(|e| e.event_type == "user/message")
        .filter_map(|e| {
            let message = starriver_kernel::llm::Message::from_json(e.data.clone()).ok()?;
            match message.content.first() {
                Some(ContentBlock::Text(t)) => Some(t.clone()),
                _ => None,
            }
        })
        .collect()
}

#[tokio::test]
async fn sp3_instructions_full_chain_and_resync() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("AGENTS.md"), "always use pnpm").unwrap();
    std::fs::write(ws.path().join("STARRIVER.local.md"), "local overlay").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("sp3.db"), bus.clone()).unwrap(),
    );
    let kernel = PluginKernel::with_scopes(
        ctx,
        bus.clone(),
        persistence,
        Arc::new(starriver_kernel::agent::AgentScopeRegistry::new()),
    );
    let systems = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    kernel.set_llm_adapter(Arc::new(SystemRecordingAdapter {
        providers: vec!["mock".into()],
        systems: Arc::clone(&systems),
    }));

    let sid = "sp3-fullchain".to_string();
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let execution = kernel
        .execute_with_meta_in(
            "steady".into(),
            Some(sid.clone()),
            None,
            tx,
            tokio_util::sync::CancellationToken::new(),
            None,
            Some(ws.path().to_string_lossy().into_owned()),
            vec![],
            None,
            None,
            None,
        )
        .await
        .unwrap();
    starriver_host::plugin_kernel::wait_terminal(execution.agent().clone(), bus.clone()).await;

    let seen = user_surface_texts(&execution);
    assert_eq!(seen[0], "steady", "claimed input first");
    assert!(
        seen[1].contains("<system-reminder>")
            && seen[1].contains("[AGENTS.md]")
            && seen[1].contains("always use pnpm")
            && seen[1].contains("[STARRIVER.local.md]")
            && seen[1].contains("local overlay"),
        "baseline carries AGENTS.md plus the STARRIVER.local.md overlay: {:?}",
        seen.get(1)
    );
    assert!(
        seen[2].contains("Current runtime context") && seen[2].contains("workspace-write"),
        "runtime-context snapshot lands last in the batch"
    );
    assert_eq!(
        seen.len(),
        4,
        "claim + reminder + RC + first time sample: {seen:?}"
    );
    assert!(
        seen[3].starts_with("Time sampled while preparing turn 1, step 1: "),
        "SP4: time-context lands last in the batch"
    );
    execution.dispose();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Resync on resume: a new instruction file produces exactly one more
    // reminder, still after the claimed input.
    std::fs::write(ws.path().join("STARRIVER.md"), "prefer rustfmt").unwrap();
    let execution = run_sp1b(&kernel, &bus, "second", &sid).await;
    let seen = user_surface_texts(&execution);
    // Resume carries the full history; the new batch is the tail.
    assert_eq!(
        seen[seen.len() - 2],
        "second",
        "claimed input first within the resumed batch: {seen:?}"
    );
    assert!(
        seen[seen.len() - 1].contains("prefer rustfmt")
            && seen[seen.len() - 1].contains("always use pnpm"),
        "resynced snapshot carries the union: {:?}",
        seen.last()
    );
    assert_eq!(
        seen.len(),
        6,
        "first batch 4 (claim+reminder+RC+time) + resync batch 2 (time \
         throttled via the persisted anchor): {seen:?}"
    );
    let total_reminders = {
        let events = execution.agent().session().events();
        events
            .iter()
            .filter(|e| e.event_type == "user/message")
            .filter(|e| format!("{:?}", e.data).contains("<system-reminder>"))
            .count()
    };
    assert_eq!(total_reminders, 2, "exactly one baseline + one resync");
    execution.dispose();
}

#[tokio::test]
async fn sp2_assembly_failure_reaches_the_ui_as_turn_error() {
    use starriver_kernel::system_prompt::{SectionEntry, SectionText};

    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("sp2.db"), bus.clone()).unwrap(),
    );
    let kernel = PluginKernel::new(ctx, bus.clone(), persistence);
    kernel
        .system_prompts()
        .section(
            &cordis_rslite::Context::new(),
            SectionEntry {
                name: "broken".into(),
                order: 10,
                complete: false,
                text: SectionText::Static("hello {{who}}".into()),
            },
        )
        .unwrap();
    let systems = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    kernel.set_llm_adapter(Arc::new(SystemRecordingAdapter {
        providers: vec!["mock".into()],
        systems: Arc::clone(&systems),
    }));

    let (tx, rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let execution = kernel
        .execute_with_meta_in(
            "hi".into(),
            Some("sp2-error".into()),
            None,
            tx,
            tokio_util::sync::CancellationToken::new(),
            None,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let seen = drain_until_terminal(rx).await;
    let error = seen.iter().find_map(|event| match event {
        UiEvent::TurnError { error, .. } => Some(error.clone()),
        _ => None,
    });
    assert!(
        error
            .as_deref()
            .is_some_and(|e| e.contains("unknown variable reference")),
        "assembly failure must surface as TurnError with the reason: {error:?}"
    );
    assert!(
        systems.lock().unwrap().is_empty(),
        "no model request may go out when assembly fails"
    );
    execution.dispose();
}

#[tokio::test]
async fn sp1b_persona_clear_restores_identity_only() {
    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("sp1b.db"), bus.clone()).unwrap(),
    );
    let kernel = PluginKernel::new(ctx, bus.clone(), persistence);
    let systems = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    kernel.set_llm_adapter(Arc::new(SystemRecordingAdapter {
        providers: vec!["mock".into()],
        systems: Arc::clone(&systems),
    }));

    kernel
        .set_persona(Some("engineer persona".into()))
        .await
        .unwrap();
    let execution = run_sp1b(&kernel, &bus, "with", "sp1b-clear-a").await;
    let expected_prefix = format!(
        "{}\n\nengineer persona",
        starriver_host::plugin_kernel::IDENTITY_PROMPT
    );
    let system = systems
        .lock()
        .unwrap()
        .last()
        .cloned()
        .flatten()
        .unwrap_or_default();
    assert!(
        system.starts_with(expected_prefix.as_str()),
        "skills empty → persona follows identity directly, before the default residents: {system:?}"
    );
    assert!(
        !system.contains("\n\n\n"),
        "empty skills must leave no separator trace: {system:?}"
    );
    execution.dispose();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    kernel.set_persona(None).await.unwrap();
    let execution = run_sp1b(&kernel, &bus, "without", "sp1b-clear-b").await;
    let system = systems
        .lock()
        .unwrap()
        .last()
        .cloned()
        .flatten()
        .unwrap_or_default();
    assert!(
        system.starts_with(starriver_host::plugin_kernel::IDENTITY_PROMPT)
            && !system.contains("engineer persona"),
        "clearing the persona removes it from the assembly: {system:?}"
    );
    assert!(
        !system.contains("\n\n\n"),
        "cleared persona must leave no separator trace: {system:?}"
    );
    execution.dispose();
}

#[tokio::test]
async fn sp4_time_context_samples_once_per_window_across_resume() {
    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("sp4.db"), bus.clone()).unwrap(),
    );
    let kernel = PluginKernel::new(ctx, bus.clone(), persistence);
    let systems = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    kernel.set_llm_adapter(Arc::new(SystemRecordingAdapter {
        providers: vec!["mock".into()],
        systems: Arc::clone(&systems),
    }));
    let time_count = |execution: &starriver_host::plugin_kernel::KernelExecution| {
        let events = execution.agent().session().events();
        events
            .iter()
            .filter(|e| e.event_type == "user/message")
            .filter(|e| format!("{:?}", e.data).contains("Time sampled while preparing"))
            .count()
    };

    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let execution = kernel
        .execute_with_meta_in(
            "a".into(),
            Some("sp4-time".into()),
            None,
            tx,
            tokio_util::sync::CancellationToken::new(),
            None,
            None,
            vec![],
            None,
            None,
            None,
        )
        .await
        .unwrap();
    starriver_host::plugin_kernel::wait_terminal(execution.agent().clone(), bus.clone()).await;
    assert_eq!(time_count(&execution), 1, "first sample is mandatory");
    execution.dispose();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let execution = run_sp1b(&kernel, &bus, "b", "sp4-time").await;
    assert_eq!(
        time_count(&execution),
        1,
        "resume inside the 60s window must not resample (persisted anchor)"
    );
    execution.dispose();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let execution = run_sp1b(&kernel, &bus, "c", "sp4-time").await;
    assert_eq!(
        time_count(&execution),
        1,
        "third turn inside the window stays silent"
    );
    execution.dispose();
}

// ── WI-115: hooks wired into the tool pipeline ──

fn kernel_with_hooks(
    path: std::path::PathBuf,
    adapter: Arc<dyn LlmAdapter>,
    hooks_dir: std::path::PathBuf,
) -> Arc<PluginKernel> {
    use starriver_host::hooks::HookEngine;
    use starriver_host::plugin_kernel::WebToolOverrides;
    use starriver_kernel::agent::AgentScopeRegistry;
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = SqliteSessionPersistence::open_with_bus(path, bus.clone()).unwrap();
    let kernel = PluginKernel::with_services_web_overrides_and_hooks(
        ctx,
        bus.clone(),
        Arc::new(persistence),
        Arc::new(AgentScopeRegistry::new()),
        WebToolOverrides::default(),
        Arc::new(HookEngine::new(Some(hooks_dir))),
    );
    kernel.set_llm_adapter(adapter);
    Arc::new(kernel)
}

fn write_hook(dir: &std::path::Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// A PreToolUse hook that denies (exit 2) must gate the tool BEFORE the
/// approval bridge is consulted, and the script must have received the full
/// env contract (event/tool/session + params file from the real execution).
#[tokio::test]
async fn pre_tool_use_hook_denies_before_approval_with_context() {
    let dir = tempfile::tempdir().unwrap();
    let hooks_dir = dir.path().join("hooks");
    let event_dir = hooks_dir.join("PreToolUse");
    std::fs::create_dir_all(&event_dir).unwrap();
    let audit = dir.path().join("audit.txt");
    write_hook(
        &event_dir,
        "all.sh",
        &format!(
            "#!/bin/sh\necho \"$STARRIVER_EVENT $STARRIVER_TOOL session=$STARRIVER_SESSION params=$(cat \"$STARRIVER_PARAMS_FILE\")\" >> {}\necho \"hook saw $STARRIVER_TOOL\"\nexit 2\n",
            audit.display()
        ),
    );
    let (adapter, _t) = tool_call_adapter_args("write", r#"{"path":"out.txt","content":"data"}"#);
    let kernel = kernel_with_hooks(dir.path().join("s.db"), adapter, hooks_dir);
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();

    // Approval watchdog: if the bridge ever receives a request, the hook did
    // NOT gate before approval — fail the test.
    let approver = {
        let kernel = Arc::clone(&kernel);
        tokio::spawn(async move {
            for _ in 0..300 {
                if kernel.approval_bridge.approve("s-hook", true, false) {
                    panic!("hook denial must bypass the approval bridge");
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
    };
    let execution = kernel
        .execute(
            "write a file".into(),
            Some("s-hook".into()),
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    approver.await.unwrap();
    let seen = drain_until_terminal(rx).await;
    let denied = seen.iter().any(
        |e| matches!(e, UiEvent::ToolResult { ok: false, output, .. } if output.contains("hook saw")),
    );
    assert!(denied, "expected a hook-denied tool result, got {seen:?}");
    execution.dispose();

    let audit_log = std::fs::read_to_string(&audit).unwrap();
    assert!(
        audit_log.contains("PreToolUse write session=s-hook"),
        "{audit_log}"
    );
    assert!(
        audit_log.contains("out.txt"),
        "params file content: {audit_log}"
    );
}

/// An observing (exit 0) PreToolUse hook leaves the tool flowing (through
/// approval), and the PostToolUse hook receives the result file.
#[tokio::test]
async fn observing_hooks_pass_through_and_post_sees_result() {
    let dir = tempfile::tempdir().unwrap();
    let hooks_dir = dir.path().join("hooks");
    let pre = hooks_dir.join("PreToolUse");
    let post = hooks_dir.join("PostToolUse");
    std::fs::create_dir_all(&pre).unwrap();
    std::fs::create_dir_all(&post).unwrap();
    let post_log = dir.path().join("post.txt");
    write_hook(&pre, "all.sh", "#!/bin/sh\nexit 0\n");
    write_hook(
        &post,
        "all.sh",
        &format!(
            "#!/bin/sh\necho \"$STARRIVER_TOOL ok=$(cat \"$STARRIVER_RESULT_FILE\")\" >> {}\nexit 0\n",
            post_log.display()
        ),
    );
    let (adapter, _t) = tool_call_adapter_args("write", r#"{"path":"out.txt","content":"data"}"#);
    let kernel = kernel_with_hooks(dir.path().join("s.db"), adapter, hooks_dir);
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let approver = {
        let kernel = Arc::clone(&kernel);
        tokio::spawn(async move {
            for _ in 0..500 {
                if kernel.approval_bridge.approve("s-flow", true, false) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("write approval never reached the bridge");
        })
    };
    let execution = kernel
        .execute(
            "write a file".into(),
            Some("s-flow".into()),
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    approver.await.unwrap();
    let seen = drain_until_terminal(rx).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: true, .. })));
    execution.dispose();
    let log = std::fs::read_to_string(&post_log).unwrap();
    assert!(log.contains("write ok="), "{log}");
    assert!(log.contains("\"ok\":true"), "result file JSON: {log}");
}
