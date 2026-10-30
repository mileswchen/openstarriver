//! MCP bridge integration and e2e: discovery, kernel registration, and
//! agent-loop execution against a local HTTP mock MCP server.

pub use std::sync::Arc;
use std::sync::Mutex;

use cordis_rslite::{Context, EventBus, Value};
use starriver_host::mcp::adapter::{McpToolInfo, TokenProvider};
use starriver_host::mcp::config::{McpServerConfig, TransportType};
use starriver_host::mcp::definition::{discover_bridge, mcp_tool_definition};
use starriver_host::plugin_kernel::PluginKernel;
use starriver_kernel::agent::AgentScopeRegistry;
use starriver_kernel::llm::{
    CallId, ContentBlock, FinishReason, GenerateOptions, LlmAdapter, LlmError, StreamChunk,
    ToolSchema,
};
use starriver_kernel::tools::{ToolDefinition, ToolExecutionInput, ToolRuntime};
use starriver_kernel::ui::UiEvent;
use starriver_session_persistence::SqliteSessionPersistence;
use starriver_tools::InMemoryToolRuntime;

/// Serve canned raw HTTP responses on 127.0.0.1, one connection each, and
/// record request bodies. `Connection: close` forces a fresh TCP connection
/// per request so the responses are consumed in order.
async fn spawn_mcp_mock(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::clone(&recorded);
    tokio::spawn(async move {
        for response in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            requests
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..n]).into_owned());
            let _ = sock.write_all(response.as_bytes()).await;
        }
    });
    (format!("http://{addr}"), recorded)
}

fn http_ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// BUG-136: discovery now POSTs `notifications/initialized` right after a
/// successful `initialize`; spec servers answer 202 with an empty body (the
/// bridge drains and ignores it), so every canned initialize sequence needs
/// this slot consumed.
fn accepted() -> String {
    "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
}

fn json_rpc_result(id: u64, result: serde_json::Value) -> String {
    http_ok(&serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
}

/// A JSON-RPC method-not-found error, used for the discovery-time
/// resources/prompts capability probes when the mock server has none.
fn method_not_found(id: u64) -> String {
    http_ok(
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "Method not found"}
        })
        .to_string(),
    )
}

fn http_config(url: String) -> McpServerConfig {
    http_config_with(url, Vec::new(), None)
}

fn http_config_with(
    url: String,
    deny_in_readonly_tools: Vec<String>,
    tool_timeout_secs: Option<u64>,
) -> McpServerConfig {
    McpServerConfig {
        transport: TransportType::Http,
        command: String::new(),
        args: vec![],
        env: Default::default(),
        url: Some(url),
        oauth: None,
        deny_in_readonly_tools,
        tool_timeout_secs,
        per_tool_timeout_secs: Default::default(),
    }
}

fn no_token() -> TokenProvider {
    Arc::new(|| None)
}

fn execution_input(name: &str, arguments: serde_json::Value) -> ToolExecutionInput {
    ToolExecutionInput {
        call_id: CallId("c1".into()),
        root_call_id: None,
        name: name.into(),
        arguments: Value::new(arguments),
        agent_id: None,
        cwd: None,
        sandbox_mode: None,
        parent: None,
        cancellation: None,
    }
}

type ScriptedMcp = (Arc<dyn LlmAdapter>, Arc<Mutex<Vec<Vec<ToolSchema>>>>);

/// Adapter that emits scripted tool calls in order, then a closing text
/// block, and records the schema list of every model request.
fn scripted_mcp_adapter(
    calls: Vec<(&'static str, String)>,
    final_text: &'static str,
) -> ScriptedMcp {
    struct ScriptedCalls {
        calls: std::sync::Mutex<std::collections::VecDeque<(&'static str, String)>>,
        final_text: &'static str,
        schemas: Arc<Mutex<Vec<Vec<ToolSchema>>>>,
    }
    impl LlmAdapter for ScriptedCalls {
        fn providers(&self) -> &[String] {
            static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            PROVIDERS.get_or_init(|| vec!["mock".to_string()])
        }
        fn stream(
            &self,
            options: GenerateOptions,
        ) -> futures::stream::BoxStream<'static, Result<StreamChunk, LlmError>> {
            let tools = options.tools.clone().unwrap_or_default();
            self.schemas.lock().unwrap().push(tools);
            let call = self.calls.lock().unwrap().pop_front();
            match call {
                Some((name, args_json)) => Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::ToolCallDelta {
                        index: 0,
                        id: CallId("c1".into()),
                        name: Some(name.into()),
                        arguments_delta: args_json.clone(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::ToolCall {
                            id: CallId("c1".into()),
                            name: Some(name.into()),
                            arguments: starriver_kernel::json::JsonValue::from_json_str(&args_json)
                                .unwrap_or_else(|_| {
                                    starriver_kernel::json::JsonValue::String(args_json.to_string())
                                }),
                        },
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::ToolCalls,
                        replay_state: None,
                    }),
                ])),
                None => {
                    let text = self.final_text;
                    Box::pin(futures::stream::iter(vec![
                        Ok(StreamChunk::TextDelta {
                            index: 0,
                            text: text.into(),
                        }),
                        Ok(StreamChunk::BlockEnd {
                            index: 0,
                            block: ContentBlock::Text(text.into()),
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::Stop,
                            replay_state: None,
                        }),
                    ]))
                }
            }
        }
    }
    let schemas = Arc::new(Mutex::new(Vec::new()));
    let adapter = Arc::new(ScriptedCalls {
        calls: std::sync::Mutex::new(calls.into()),
        final_text,
        schemas: Arc::clone(&schemas),
    }) as Arc<dyn LlmAdapter>;
    (adapter, schemas)
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

fn ping_definition(url: String) -> ToolDefinition {
    mcp_tool_definition(
        "mock".into(),
        McpToolInfo {
            name: "ping".into(),
            description: "Ping".into(),
            input_schema: serde_json::json!({"type": "object"}),
            annotations: Default::default(),
        },
        http_config(url),
        no_token(),
    )
}

async fn kernel_with_mcp(
    path: std::path::PathBuf,
    definitions: Vec<ToolDefinition>,
) -> PluginKernel {
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(SqliteSessionPersistence::open_with_bus(path, bus.clone()).unwrap());
    let scopes = Arc::new(AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(ctx, bus.clone(), persistence, scopes);
    if !definitions.is_empty() {
        kernel
            .register_mcp_definitions("mock", definitions)
            .await
            .unwrap();
    }
    kernel
}

#[tokio::test]
async fn register_mcp_definitions_makes_tools_visible_and_unregister_removes_them() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = kernel_with_mcp(
        dir.path().join("s.db"),
        vec![ping_definition("http://127.0.0.1:1".into())],
    )
    .await;
    let names: Vec<String> = kernel
        .tools()
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect();
    assert!(names.contains(&"mcp_mock_ping".to_string()));

    kernel.unregister_mcp_definitions("mock").await;
    let names: Vec<String> = kernel
        .tools()
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect();
    assert!(!names.contains(&"mcp_mock_ping".to_string()));
}

#[tokio::test]
async fn re_register_same_server_replaces_old_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = kernel_with_mcp(
        dir.path().join("s.db"),
        vec![ping_definition("http://127.0.0.1:1".into())],
    )
    .await;
    let pong = mcp_tool_definition(
        "mock".into(),
        McpToolInfo {
            name: "pong".into(),
            description: "Pong".into(),
            input_schema: serde_json::json!({"type": "object"}),
            annotations: Default::default(),
        },
        http_config("http://127.0.0.1:1".into()),
        no_token(),
    );
    kernel
        .register_mcp_definitions("mock", vec![pong])
        .await
        .unwrap();
    let names: Vec<String> = kernel
        .tools()
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect();
    assert!(names.contains(&"mcp_mock_pong".to_string()));
    assert!(!names.contains(&"mcp_mock_ping".to_string()));
}

#[tokio::test]
async fn agent_loop_executes_mcp_tool_end_to_end() {
    let (url, requests) = spawn_mcp_mock(vec![json_rpc_result(
        1,
        serde_json::json!({"content": [{"type": "text", "text": "news: hello"}]}),
    )])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("s.db"), bus.clone()).unwrap(),
    );
    let scopes = Arc::new(AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(ctx, bus.clone(), persistence, scopes);
    kernel.set_preset(starriver_host::preset::Preset {
        sandbox: starriver_kernel::sandbox::SandboxMode::DangerFullAccess,
        approval: starriver_agent_loop::approval::ApprovalMode::Manual,
    });
    let news = mcp_tool_definition(
        "mock".into(),
        McpToolInfo {
            name: "get_hot_news".into(),
            description: "Get hot news".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"limit": {"type": "integer"}},
            }),
            annotations: Default::default(),
        },
        http_config(url),
        no_token(),
    );
    kernel
        .register_mcp_definitions("mock", vec![news])
        .await
        .unwrap();
    let (adapter, seen) = scripted_mcp_adapter(
        vec![("mcp_mock_get_hot_news", r#"{"limit":5}"#.to_string())],
        "done",
    );
    kernel.set_llm_adapter(adapter);

    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let execution = kernel
        .execute(
            "go".into(),
            None,
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let events = drain_until_terminal(rx).await;

    let all_schemas = seen.lock().unwrap();
    let mcp_schema = all_schemas
        .iter()
        .flatten()
        .find(|schema| schema.name == "mcp_mock_get_hot_news")
        .expect("model must see the MCP tool");
    assert!(mcp_schema.parameters.get("properties").is_some());
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::ToolResult { ok: true, output, .. } if output.contains("news: hello")
    )));
    let bodies = requests.lock().unwrap();
    assert!(bodies.last().unwrap().contains("\"name\":\"get_hot_news\""));
    execution.dispose();
}

#[tokio::test]
async fn shared_connection_serves_all_tools_of_one_server() {
    let (url, requests) = spawn_mcp_mock(vec![
        json_rpc_result(
            1,
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "serverInfo": {"name": "mock", "version": "0.1.0"},
            }),
        ),
        accepted(),
        json_rpc_result(
            2,
            serde_json::json!({
                "tools": [
                    {"name": "one", "description": "one", "inputSchema": {"type": "object"}},
                    {"name": "two", "description": "two", "inputSchema": {"type": "object"}},
                ]
            }),
        ),
        method_not_found(3),
        method_not_found(4),
        json_rpc_result(
            3,
            serde_json::json!({"content": [{"type": "text", "text": "one done"}]}),
        ),
        json_rpc_result(
            4,
            serde_json::json!({"content": [{"type": "text", "text": "two done"}]}),
        ),
    ])
    .await;

    let definitions = discover_bridge("mock", &http_config(url), no_token())
        .await
        .unwrap();
    assert_eq!(definitions.len(), 2);
    let runtime = InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let mut handles = Vec::new();
    for definition in definitions {
        handles.push(runtime.register(definition).unwrap());
    }

    let first = runtime
        .execute(execution_input("mcp_mock_one", serde_json::json!({})))
        .await
        .unwrap();
    assert!(!first.is_error());
    assert!(first.content().iter().any(|block| matches!(
        block,
        starriver_kernel::llm::ContentBlock::Text(text) if text == "one done"
    )));

    let second = runtime
        .execute(execution_input("mcp_mock_two", serde_json::json!({})))
        .await
        .unwrap();
    assert!(!second.is_error());
    assert!(second.content().iter().any(|block| matches!(
        block,
        starriver_kernel::llm::ContentBlock::Text(text) if text == "two done"
    )));

    let bodies = requests.lock().unwrap();
    assert!(bodies.iter().any(|body| body.contains("\"name\":\"one\"")));
    assert!(bodies.iter().any(|body| body.contains("\"name\":\"two\"")));
}

#[tokio::test]
async fn readonly_sandbox_denies_declared_write_tools_before_approval() {
    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("s.db"), bus.clone()).unwrap(),
    );
    let scopes = Arc::new(AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(ctx, bus.clone(), persistence, scopes);
    kernel.set_preset(starriver_host::preset::Preset {
        sandbox: starriver_kernel::sandbox::SandboxMode::ReadOnly,
        approval: starriver_agent_loop::approval::ApprovalMode::Manual,
    });
    let push = mcp_tool_definition(
        "mock".into(),
        McpToolInfo {
            name: "git_push".into(),
            description: "push".into(),
            input_schema: serde_json::json!({"type": "object"}),
            annotations: Default::default(),
        },
        http_config_with(
            "http://127.0.0.1:1".into(),
            vec!["git_push".into()],
            Some(30),
        ),
        no_token(),
    );
    assert!(push.deny_in_readonly);
    assert_eq!(push.timeout, Some(std::time::Duration::from_secs(30)));
    kernel
        .register_mcp_definitions("mock", vec![push])
        .await
        .unwrap();

    let (adapter, _seen) =
        scripted_mcp_adapter(vec![("mcp_mock_git_push", "{}".to_string())], "done");
    kernel.set_llm_adapter(adapter);
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let execution = kernel
        .execute(
            "go".into(),
            None,
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let events = drain_until_terminal(rx).await;
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::ToolResult { ok: false, output, .. } if output.contains("read-only sandbox")
    )));
    execution.dispose();
}

#[tokio::test]
async fn danger_full_access_executes_declared_write_tool() {
    let (url, _) = spawn_mcp_mock(vec![json_rpc_result(
        1,
        serde_json::json!({"content": [{"type": "text", "text": "push ok"}]}),
    )])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("s.db"), bus.clone()).unwrap(),
    );
    let scopes = Arc::new(AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(ctx, bus.clone(), persistence, scopes);
    kernel.set_preset(starriver_host::preset::Preset {
        sandbox: starriver_kernel::sandbox::SandboxMode::DangerFullAccess,
        approval: starriver_agent_loop::approval::ApprovalMode::Manual,
    });
    let push = mcp_tool_definition(
        "mock".into(),
        McpToolInfo {
            name: "git_push".into(),
            description: "push".into(),
            input_schema: serde_json::json!({"type": "object"}),
            annotations: Default::default(),
        },
        http_config_with(url, vec!["git_push".into()], None),
        no_token(),
    );
    kernel
        .register_mcp_definitions("mock", vec![push])
        .await
        .unwrap();

    let (adapter, _seen) =
        scripted_mcp_adapter(vec![("mcp_mock_git_push", "{}".to_string())], "done");
    kernel.set_llm_adapter(adapter);
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let execution = kernel
        .execute(
            "go".into(),
            None,
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let events = drain_until_terminal(rx).await;
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::ToolResult { ok: true, output, .. } if output.contains("push ok")
    )));
    execution.dispose();
}

#[tokio::test]
async fn discover_bridge_and_execute_mcp_tool_through_new_seam() {
    let (url, requests) = spawn_mcp_mock(vec![
        json_rpc_result(
            1,
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "serverInfo": {"name": "mock", "version": "0.1.0"},
            }),
        ),
        accepted(),
        json_rpc_result(
            2,
            serde_json::json!({
                "tools": [{
                    "name": "get_hot_news",
                    "description": "Get hot news",
                    "inputSchema": {"type":"object","properties":{"limit":{"type":"integer"}}}
                }]
            }),
        ),
        method_not_found(3),
        method_not_found(4),
        json_rpc_result(
            3,
            serde_json::json!({
                "content": [{"type": "text", "text": "news: hello"}],
                "structuredContent": {"items": 1},
            }),
        ),
    ])
    .await;

    let mut definitions = discover_bridge("mock", &http_config(url), no_token())
        .await
        .unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "mcp_mock_get_hot_news");

    let runtime = InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let _handle = runtime.register(definitions.remove(0)).unwrap();

    let result = runtime
        .execute(execution_input(
            "mcp_mock_get_hot_news",
            serde_json::json!({"limit": 5}),
        ))
        .await
        .unwrap();
    assert!(!result.is_error());
    let content = result.content();
    assert!(content.iter().any(|block| matches!(
        block,
        starriver_kernel::llm::ContentBlock::Text(text) if text == "news: hello"
    )));

    let bodies = requests.lock().unwrap();
    let call_body = bodies.last().unwrap();
    assert!(call_body.contains("\"method\":\"tools/call\""));
    assert!(call_body.contains("\"name\":\"get_hot_news\""));
    assert!(call_body.contains("\"arguments\":{\"limit\":5}"));
}

#[tokio::test]
async fn mcp_is_error_result_becomes_tool_failure() {
    let (url, _) = spawn_mcp_mock(vec![
        json_rpc_result(1, serde_json::json!({})),
        accepted(),
        json_rpc_result(
            2,
            serde_json::json!({"tools": [{"name": "boom", "description": "", "inputSchema": {}}]}),
        ),
        method_not_found(3),
        method_not_found(4),
        json_rpc_result(
            3,
            serde_json::json!({
                "isError": true,
                "content": [{"type": "text", "text": "boom happened"}],
            }),
        ),
    ])
    .await;

    let mut definitions = discover_bridge("mock", &http_config(url), no_token())
        .await
        .unwrap();
    let runtime = InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let _handle = runtime.register(definitions.remove(0)).unwrap();

    let result = runtime
        .execute(execution_input("mcp_mock_boom", serde_json::json!({})))
        .await
        .unwrap();
    assert!(result.is_error());
    let error = result.error().unwrap();
    assert_eq!(error.code, "MCP_IS_ERROR");
    assert!(error.message.contains("boom happened"));
}

#[tokio::test]
async fn mcp_transport_error_becomes_tool_failure() {
    let (url, _) = spawn_mcp_mock(vec![
        json_rpc_result(1, serde_json::json!({})),
        accepted(),
        json_rpc_result(
            2,
            serde_json::json!({"tools": [{"name": "boom", "description": "", "inputSchema": {}}]}),
        ),
        method_not_found(3),
        method_not_found(4),
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string(),
    ])
    .await;

    let mut definitions = discover_bridge("mock", &http_config(url), no_token())
        .await
        .unwrap();
    let runtime = InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let _handle = runtime.register(definitions.remove(0)).unwrap();

    let result = runtime
        .execute(execution_input("mcp_mock_boom", serde_json::json!({})))
        .await
        .unwrap();
    assert!(result.is_error());
    assert_eq!(result.error().unwrap().code, "MCP_CALL_FAILED");
}

/// Shell used to run the stdio mock servers: /bin/bash on Unix, git-bash's
/// `bash` from PATH on Windows (tokio's spawn searches PATH properly).
fn mock_shell() -> String {
    if cfg!(windows) {
        "bash".into()
    } else {
        "/bin/bash".into()
    }
}

/// A stdio mock MCP server: one JSON-RPC response per stdin line. `slow_tool`
/// sleeps 5s before answering; everything else answers immediately.
const SLOW_STDIO_MOCK: &str = r#"while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*) echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"slow-mock"}}}';;
    *'"tools/list"'*) echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"slow_tool","description":"slow","inputSchema":{"type":"object"}},{"name":"fast_tool","description":"fast","inputSchema":{"type":"object"}}]}}';;
    *'"resources/list"'*|*'"prompts/list"'*) echo '{"jsonrpc":"2.0","id":9,"error":{"code":-32601,"message":"Method not found"}}';;
    *'"tools/call"'*)
      if echo "$line" | grep -q '"slow_tool"'; then
        sleep 5
        echo '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"slow done"}]}}'
      else
        echo '{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"fast done"}]}}'
      fi;;
  esac
done
"#;

#[tokio::test]
async fn slow_stdio_tool_is_aborted_by_per_tool_timeout_and_connection_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("mock_mcp.sh");
    std::fs::write(&script, SLOW_STDIO_MOCK).unwrap();
    let mut config = McpServerConfig {
        transport: TransportType::Stdio,
        command: mock_shell(),
        args: vec![script.to_string_lossy().into_owned()],
        env: Default::default(),
        url: None,
        oauth: None,
        deny_in_readonly_tools: Vec::new(),
        // Server-level default is generous; only slow_tool is tight.
        tool_timeout_secs: Some(60),
        per_tool_timeout_secs: [("slow_tool".to_string(), 1u64)].into(),
    };

    let mut definitions = discover_bridge("mock", &config, no_token()).await.unwrap();
    assert_eq!(definitions.len(), 2);
    let runtime = InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let mut handles = Vec::new();
    for definition in definitions.drain(..) {
        handles.push(runtime.register(definition).unwrap());
    }

    // The slow tool (mock sleeps 5s, budget 1s) is aborted mid-flight, well
    // before the sleep completes.
    let started = std::time::Instant::now();
    let result = runtime
        .execute(execution_input("mcp_mock_slow_tool", serde_json::json!({})))
        .await
        .unwrap();
    assert!(result.is_error());
    assert_eq!(result.error().unwrap().code, "MCP_TIMEOUT");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "call must be aborted, not waited out (elapsed {:?})",
        started.elapsed()
    );

    // The aborted call desynchronized the stdio stream: the connection must
    // have been reset so fast_tool works again on a fresh subprocess
    // (relaunch + initialize + call).
    let result = runtime
        .execute(execution_input("mcp_mock_fast_tool", serde_json::json!({})))
        .await
        .unwrap();
    assert!(!result.is_error());
    assert!(result.content().iter().any(|block| matches!(
        block,
        starriver_kernel::llm::ContentBlock::Text(text) if text == "fast done"
    )));
    drop(handles);
}

/// A stdio mock MCP server that dies ONCE on `ping` (when the marker file
/// named by $MOCK_DIE_MARKER exists — consumed so subsequent pings succeed),
/// then serves normally after being relaunched.
const DYING_STDIO_MOCK: &str = r#"while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*) echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"dying-mock"}}}';;
    *'"tools/list"'*) echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"ping_tool","description":"p","inputSchema":{"type":"object"}}]}}';;
    *'"resources/list"'*|*'"prompts/list"'*) echo '{"jsonrpc":"2.0","id":9,"error":{"code":-32601,"message":"Method not found"}}';;
    *'"ping"'*)
      if [ -f "$MOCK_DIE_MARKER" ]; then
        rm -f "$MOCK_DIE_MARKER"
        exit 0
      fi
      echo '{"jsonrpc":"2.0","id":3,"result":{}}';;
    *'"tools/call"'*) echo '{"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"pong"}]}}';;
  esac
done
"#;

#[tokio::test]
async fn dead_stdio_subprocess_is_restarted_and_recovers_by_health_monitor() {
    use starriver_host::mcp::health::{HealthState, McpHealthMonitor, RestartPolicy};

    let dir = tempfile::tempdir().unwrap();
    let die_marker = dir.path().join("die-once");
    std::fs::write(&die_marker, "1").unwrap();
    let script = dir.path().join("mock_mcp.sh");
    std::fs::write(&script, DYING_STDIO_MOCK).unwrap();
    let config = McpServerConfig {
        transport: TransportType::Stdio,
        command: mock_shell(),
        args: vec![script.to_string_lossy().into_owned()],
        env: [(
            "MOCK_DIE_MARKER".to_string(),
            die_marker.to_string_lossy().into_owned(),
        )]
        .into(),
        url: None,
        oauth: None,
        deny_in_readonly_tools: Vec::new(),
        tool_timeout_secs: None,
        per_tool_timeout_secs: Default::default(),
    };

    let (mut definitions, connection) =
        starriver_host::mcp::definition::discover_bridge_shared("mock", &config, no_token())
            .await
            .unwrap();
    assert_eq!(definitions.len(), 1);
    let runtime = InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let _handle = runtime.register(definitions.remove(0)).unwrap();

    // Baseline: the tool works on the live subprocess.
    let result = runtime
        .execute(execution_input("mcp_mock_ping_tool", serde_json::json!({})))
        .await
        .unwrap();
    assert!(!result.is_error());

    let recoveries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&recoveries);
    let monitor = McpHealthMonitor::new("mock".into(), config, no_token(), connection)
        .with_policy(RestartPolicy {
            ping_interval: std::time::Duration::from_millis(100),
            ping_timeout: std::time::Duration::from_secs(2),
            initial_backoff: std::time::Duration::from_millis(50),
            max_backoff: std::time::Duration::from_millis(200),
            max_attempts: 5,
        })
        .on_recovery(Arc::new(move |_definitions| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {})
        }));
    let handle = monitor.start();
    assert_eq!(handle.state(), HealthState::Healthy);

    // The subprocess dies on the next ping; the monitor must relaunch it and
    // swap the connection in place.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while recoveries.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "monitor did not restart the dead subprocess in time (state {:?})",
            handle.state()
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(handle.state(), HealthState::Healthy);

    // The original definition still executes: its executor shares the
    // swapped-in connection (fresh subprocess, re-handshaken).
    let result = runtime
        .execute(execution_input("mcp_mock_ping_tool", serde_json::json!({})))
        .await
        .unwrap();
    assert!(!result.is_error());
    assert!(result.content().iter().any(|block| matches!(
        block,
        starriver_kernel::llm::ContentBlock::Text(text) if text == "pong"
    )));
    handle.stop();
}

#[tokio::test]
async fn health_monitor_gives_up_after_max_attempts_and_marks_dead() {
    use starriver_host::mcp::health::{HealthState, McpHealthMonitor, RestartPolicy};
    use starriver_host::mcp::transport::{Connection, HttpConnection};

    // A launched-by-construction HTTP connection to a closed local port:
    // every ping and every restart attempt fails instantly (connection
    // refused on 127.0.0.1, no external network involved).
    let mut http = HttpConnection::new("mock".into(), no_token());
    http.set_url("http://127.0.0.1:1".into());
    let connection = Arc::new(tokio::sync::Mutex::new(Connection::Http(http)));
    let dead_detail: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let config = McpServerConfig {
        transport: TransportType::Http,
        command: String::new(),
        args: vec![],
        env: Default::default(),
        url: Some("http://127.0.0.1:1".into()),
        oauth: None,
        deny_in_readonly_tools: Vec::new(),
        tool_timeout_secs: None,
        per_tool_timeout_secs: Default::default(),
    };
    let monitor = McpHealthMonitor::new("mock".into(), config, no_token(), connection)
        .with_policy(RestartPolicy {
            ping_interval: std::time::Duration::from_millis(50),
            ping_timeout: std::time::Duration::from_millis(200),
            initial_backoff: std::time::Duration::from_millis(10),
            max_backoff: std::time::Duration::from_millis(20),
            max_attempts: 2,
        })
        // WI-163: give-up must surface through the dead hook (the host marks
        // the server unavailable and hands over to discovery retry).
        .on_dead({
            let fired = Arc::clone(&dead_detail);
            Arc::new(move |detail| {
                *fired.lock().unwrap() = Some(detail);
                Box::pin(async {})
            })
        });
    let handle = monitor.start();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while handle.state() != HealthState::Dead {
        assert!(
            std::time::Instant::now() < deadline,
            "monitor never marked the server dead (state {:?})",
            handle.state()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    handle.stop();
    let detail = dead_detail.lock().unwrap().clone();
    assert!(
        detail
            .as_deref()
            .is_some_and(|d| d.contains("giving up after 2 attempts")),
        "dead hook carried the give-up detail: {detail:?}"
    );
}

/// WI-163: a stateful HTTP mock — the first `fail_first` initialize
/// attempts answer 500; afterwards a full working handshake answers by
/// method (initialize → 202 → tools/list → probes/pings). Serves unlimited
/// connections, unlike the canned `spawn_mcp_mock`.
async fn spawn_flaky_mcp(fail_first: usize) -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let inits = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let inits = Arc::clone(&inits);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16384];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let response = if req.contains("\"method\":\"initialize\"") {
                    if inits.fetch_add(1, Ordering::SeqCst) < fail_first {
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    } else {
                        http_ok(
                            &serde_json::json!({
                                "jsonrpc": "2.0", "id": 1, "result": {
                                    "protocolVersion": "2024-11-05",
                                    "capabilities": {},
                                    "serverInfo": {"name": "flaky", "version": "0.1.0"}
                                }
                            })
                            .to_string(),
                        )
                    }
                } else if req.contains("notifications/initialized") {
                    accepted()
                } else if req.contains("\"method\":\"tools/list\"") {
                    http_ok(
                        &serde_json::json!({
                            "jsonrpc": "2.0", "id": 2, "result": {"tools": [
                                {"name": "ping_tool", "description": "p", "inputSchema": {}}
                            ]}
                        })
                        .to_string(),
                    )
                } else {
                    // Capability probes and pings: method-not-found suffices.
                    method_not_found(9)
                };
                let _ = sock.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

/// WI-163: a discovery that failed at startup keeps retrying in the
/// background and recovers as soon as the server comes back — the on-success
/// hook receives the discovered definitions exactly once and the handle
/// flips to Healthy.
#[tokio::test]
async fn discovery_retry_recovers_after_transient_failures() {
    use starriver_host::mcp::health::{HealthState, McpDiscoveryRetry, RestartPolicy};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let url = spawn_flaky_mcp(2).await;
    let config = http_config(url);

    let successes = Arc::new(AtomicUsize::new(0));
    let discovered_names = Arc::new(Mutex::new(Vec::<String>::new()));
    let hook_successes = Arc::clone(&successes);
    let hook_names = Arc::clone(&discovered_names);
    let retry = McpDiscoveryRetry::new("flaky".into(), config, no_token())
        .with_policy(RestartPolicy {
            ping_interval: std::time::Duration::from_millis(50),
            ping_timeout: std::time::Duration::from_millis(200),
            initial_backoff: std::time::Duration::from_millis(30),
            max_backoff: std::time::Duration::from_millis(80),
            max_attempts: usize::MAX,
        })
        .on_success(Arc::new(move |definitions, _connection| {
            hook_successes.fetch_add(1, Ordering::SeqCst);
            hook_names
                .lock()
                .unwrap()
                .extend(definitions.iter().map(|d| d.name.clone()));
            Box::pin(async {})
        }));
    assert_eq!(
        retry.state(),
        HealthState::Restarting,
        "a not-yet-discovered server reports Restarting"
    );
    let handle = retry.start_on(&tokio::runtime::Handle::current());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while handle.state() != HealthState::Healthy {
        assert!(
            std::time::Instant::now() < deadline,
            "retry never recovered (state {:?})",
            handle.state()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    handle.stop();
    assert_eq!(successes.load(Ordering::SeqCst), 1);
    assert_eq!(
        discovered_names.lock().unwrap().clone(),
        vec!["mcp_flaky_ping_tool".to_string()]
    );
}

#[tokio::test]
async fn resources_and_prompts_surfaces_bridge_into_tool_seam() {
    // Discovery: initialize, tools/list, then the capability probes answer
    // with real surfaces (one tool + resources + prompts). The final canned
    // response serves the later resources/read call.
    let (url, requests) = spawn_mcp_mock(vec![
        json_rpc_result(
            1,
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"resources": {}, "prompts": {}},
                "serverInfo": {"name": "mock", "version": "0.1.0"},
            }),
        ),
        accepted(),
        json_rpc_result(2, serde_json::json!({"tools": []})),
        json_rpc_result(
            3,
            serde_json::json!({"resources": [
                {"uri": "memo://guide", "name": "guide", "mimeType": "text/markdown"}
            ]}),
        ),
        json_rpc_result(
            4,
            serde_json::json!({"prompts": [
                {"name": "review", "description": "Review code", "arguments": []}
            ]}),
        ),
        json_rpc_result(
            5,
            serde_json::json!({"contents": [
                {"uri": "memo://guide", "mimeType": "text/markdown", "text": "# Guide\nbe careful"}
            ]}),
        ),
        json_rpc_result(
            6,
            serde_json::json!({"messages": [
                {"role": "user", "content": {"type": "text", "text": "review this diff"}}
            ]}),
        ),
    ])
    .await;

    let (definitions, _connection) = starriver_host::mcp::definition::discover_bridge_shared(
        "mock",
        &http_config(url),
        no_token(),
    )
    .await
    .unwrap();
    let names: Vec<&str> = definitions.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "mcp_mock_list_resources",
            "mcp_mock_read_resource",
            "mcp_mock_list_prompts",
            "mcp_mock_get_prompt",
        ],
        "tools list is empty; only the capability bridge tools remain"
    );
    for definition in &definitions {
        assert!(
            !definition.requires_approval,
            "read-only protocol surfaces must not require approval"
        );
        assert!(!definition.deny_in_readonly);
    }

    let runtime = InMemoryToolRuntime::new(EventBus::new(), Context::new());
    let mut handles = Vec::new();
    for definition in definitions {
        handles.push(runtime.register(definition).unwrap());
    }

    let read = runtime
        .execute(execution_input(
            "mcp_mock_read_resource",
            serde_json::json!({"uri": "memo://guide"}),
        ))
        .await
        .unwrap();
    assert!(!read.is_error());
    assert!(read.content().iter().any(|block| matches!(
        block,
        starriver_kernel::llm::ContentBlock::Text(text) if text.contains("# Guide")
    )));

    let prompt = runtime
        .execute(execution_input(
            "mcp_mock_get_prompt",
            serde_json::json!({"name": "review", "arguments": {}}),
        ))
        .await
        .unwrap();
    assert!(!prompt.is_error());
    assert!(prompt.content().iter().any(|block| matches!(
        block,
        starriver_kernel::llm::ContentBlock::Text(text)
            if text == "user: review this diff"
    )));

    let bodies = requests.lock().unwrap();
    assert!(bodies
        .iter()
        .any(|body| body.contains("\"method\":\"resources/read")
            && body.contains("\"uri\":\"memo://guide\"")));
    assert!(bodies
        .iter()
        .any(|body| body.contains("\"method\":\"prompts/get")));
    drop(handles);
}

#[tokio::test]
async fn auto_mode_auto_approves_read_only_annotated_mcp_tool() {
    // Write-semantics negotiation: the server declares readOnlyHint, so the
    // bridged definition carries requires_approval=false and the Auto-mode
    // agent runs it without any approval round-trip.
    let (url, _) = spawn_mcp_mock(vec![json_rpc_result(
        1,
        serde_json::json!({"content": [{"type": "text", "text": "repos: []"}]}),
    )])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("s.db"), bus.clone()).unwrap(),
    );
    let scopes = Arc::new(AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(ctx, bus.clone(), persistence, scopes);
    kernel.set_preset(starriver_host::preset::Preset {
        sandbox: starriver_kernel::sandbox::SandboxMode::DangerFullAccess,
        approval: starriver_agent_loop::approval::ApprovalMode::Auto,
    });
    let search = mcp_tool_definition(
        "mock".into(),
        McpToolInfo {
            name: "search_repos".into(),
            description: "Search repositories".into(),
            input_schema: serde_json::json!({"type": "object"}),
            annotations: starriver_host::mcp::write_semantics::McpToolAnnotations {
                read_only_hint: Some(true),
                ..Default::default()
            },
        },
        http_config(url),
        no_token(),
    );
    assert!(!search.requires_approval);
    kernel
        .register_mcp_definitions("mock", vec![search])
        .await
        .unwrap();

    let (adapter, _seen) =
        scripted_mcp_adapter(vec![("mcp_mock_search_repos", "{}".to_string())], "done");
    kernel.set_llm_adapter(adapter);
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let mut rx = tx.subscribe();
    let execution = kernel
        .execute(
            "go".into(),
            None,
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let events = drain_until_terminal(rx).await;
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::ToolResult { ok: true, output, .. } if output.contains("repos: []")
    )));
    execution.dispose();
}
