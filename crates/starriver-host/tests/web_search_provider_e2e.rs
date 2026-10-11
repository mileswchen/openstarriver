//! web_search provider integration and e2e: keyed backends (Exa / Perplexity /
//! DeepSeek) against local HTTP mocks, plus the agent-loop path.

pub use std::sync::Arc;
use std::sync::Mutex;

use cordis_rslite::{Context, EventBus};
use starriver_host::plugin_kernel::{PluginKernel, WebToolOverrides};
use starriver_host::tools::web_search::WebSearch;
use starriver_host::tools::web_search_provider::{
    DeepSeekSearchProvider, ExaSearchProvider, HtmlSearchProvider, PerplexitySearchProvider,
    WebSearchProvider,
};
use starriver_kernel::agent::AgentScopeRegistry;
use starriver_kernel::llm::{
    CallId, ContentBlock, FinishReason, GenerateOptions, LlmAdapter, LlmError, StreamChunk,
    ToolSchema,
};
use starriver_kernel::ui::UiEvent;
use starriver_session_persistence::SqliteSessionPersistence;

/// Serve canned raw HTTP responses on 127.0.0.1, one connection each, and
/// record request heads (headers + body).
async fn spawn_mock(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
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
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn json_body(body: &serde_json::Value) -> String {
    http_ok(&body.to_string())
}

async fn run_tool(search: WebSearch, args: serde_json::Value) -> serde_json::Value {
    let def = starriver_host::tools::web_search::web_search_definition_with(search);
    let execution = starriver_kernel::tools::ToolExecution {
        call_id: CallId("c1".into()),
        root_call_id: CallId("c1".into()),
        name: "web_search".into(),
        arguments: cordis_rslite::Value::new(args.clone()),
        agent_id: None,
        cwd: None,
        sandbox_mode: None,
        parent: None,
        token: starriver_kernel::tools::mint_tool_execution_token(),
        cancellation: None,
    };
    let result = def
        .executor
        .execute(
            cordis_rslite::Value::new(args),
            starriver_kernel::tools::ToolRunContext::new(execution),
        )
        .await
        .unwrap();
    result
        .downcast::<serde_json::Value>()
        .unwrap()
        .as_ref()
        .clone()
}

#[tokio::test]
async fn exa_provider_hits_mock_endpoint_and_renders_markdown() {
    let (url, requests) = spawn_mock(vec![json_body(&serde_json::json!({
        "results": [{
            "url": "https://a",
            "title": "Rust",
            "highlights": ["A language"],
            "publishedDate": "2026-02-02",
        }]
    }))])
    .await;
    let search = WebSearch::with_providers(vec![Arc::new(ExaSearchProvider::new(
        "exa-key".into(),
        url,
        1,
    ))]);
    let output = run_tool(search, serde_json::json!({"queries": ["rust"]})).await;
    let text = output["output"].as_str().unwrap();
    assert!(text.contains("- [Rust](https://a) — A language (2026-02-02)"));
    let head = requests.lock().unwrap()[0].to_lowercase();
    assert!(head.contains("post /search"));
    assert!(head.contains("x-api-key: exa-key"));
    assert!(head.contains("\"numresults\":5") || head.contains("\"numResults\":5"));
}

#[tokio::test]
async fn perplexity_provider_hits_mock_endpoint_and_renders_answer() {
    let (url, requests) = spawn_mock(vec![json_body(&serde_json::json!({
        "choices": [{"message": {"content": "The answer is Rust."}}],
        "search_results": [
            {"url": "https://rust-lang.org", "title": "Rust", "snippet": "Systems language", "date": "2026-01-01"},
        ],
    }))])
    .await;
    let search = WebSearch::with_providers(vec![Arc::new(PerplexitySearchProvider::new(
        "pplx-key".into(),
        url,
    ))]);
    let output = run_tool(search, serde_json::json!({"queries": ["rust"]})).await;
    let text = output["output"].as_str().unwrap();
    assert!(text.contains("The answer is Rust."));
    assert!(text.contains("- [Rust](https://rust-lang.org) — Systems language (2026-01-01)"));
    let head = requests.lock().unwrap()[0].to_lowercase();
    assert!(head.contains("post /chat/completions"));
    assert!(head.contains("authorization: bearer pplx-key"));
}

#[tokio::test]
async fn deepseek_provider_hits_mock_endpoint_and_renders_sources() {
    let (url, requests) = spawn_mock(vec![json_body(&serde_json::json!({
        "content": [
            {"type": "text", "text": "see sources", "citations": [
                {"url": "https://a", "cited_text": "snippet a"}
            ]},
            {"type": "web_search_tool_result", "content": [
                {"type": "web_search_result", "url": "https://a", "title": "A", "page_age": "2026-01-01"},
            ]},
        ]
    }))])
    .await;
    let search = WebSearch::with_providers(vec![Arc::new(DeepSeekSearchProvider::new(
        "ds-key".into(),
        url,
        "deepseek-v4-flash".into(),
    ))]);
    let output = run_tool(search, serde_json::json!({"queries": ["rust"]})).await;
    let text = output["output"].as_str().unwrap();
    assert!(text.contains("- [A](https://a) — snippet a (2026-01-01)"));
    let head = requests.lock().unwrap()[0].to_lowercase();
    assert!(head.contains("post /messages"));
    assert!(head.contains("x-api-key: ds-key"));
    assert!(head.contains("anthropic-version: 2023-06-01"));
    assert!(head.contains("\"type\":\"web_search_20250305\""));
}

#[tokio::test]
async fn html_provider_batch_queries_merge_in_order() {
    let (url, requests) = spawn_mock(vec![
        http_ok(r#"<a class="result__a" href="//x/l/?uddg=https%3A%2F%2Frust-lang.org">Rust</a>"#),
        http_ok(r#"<a class="result__a" href="//x/l/?uddg=https%3A%2F%2Ftokio.rs">Tokio</a>"#),
    ])
    .await;
    let search =
        WebSearch::with_providers(vec![Arc::new(HtmlSearchProvider::new(url.clone(), url))]);
    let output = run_tool(search, serde_json::json!({"queries": ["rust", "tokio"]})).await;
    let text = output["output"].as_str().unwrap();
    assert!(text.contains("[Rust](https://rust-lang.org)"));
    assert!(text.contains("[Tokio](https://tokio.rs)"));
    assert!(requests.lock().unwrap().len() == 2);
}

// ── Agent-loop e2e ──

type Scripted = (Arc<dyn LlmAdapter>, Arc<Mutex<Vec<Vec<ToolSchema>>>>);

fn scripted_adapter(calls: Vec<(&'static str, String)>, final_text: &'static str) -> Scripted {
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

#[tokio::test]
async fn agent_loop_sees_queries_schema_and_renders_markdown_result() {
    let (url, _requests) = spawn_mock(vec![
        http_ok(r#"<a class="result__a" href="//x/l/?uddg=https%3A%2F%2Frust-lang.org">Rust</a>"#),
        http_ok(r#"<a class="result__a" href="//x/l/?uddg=https%3A%2F%2Ftokio.rs">Tokio</a>"#),
    ])
    .await;
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(
            tempfile::tempdir().unwrap().path().join("s.db"),
            bus.clone(),
        )
        .unwrap(),
    );
    let scopes = Arc::new(AgentScopeRegistry::new());
    let web_search = starriver_host::tools::web_search::web_search_definition_with(
        WebSearch::with_providers(vec![Arc::new(HtmlSearchProvider::new(url.clone(), url))]),
    );
    let kernel = PluginKernel::with_services_and_web_overrides(
        ctx,
        bus.clone(),
        persistence,
        scopes,
        WebToolOverrides {
            web_fetch: None,
            web_search: Some(web_search),
        },
    );
    kernel.set_preset(starriver_host::preset::Preset {
        sandbox: starriver_kernel::sandbox::SandboxMode::DangerFullAccess,
        approval: starriver_agent_loop::approval::ApprovalMode::Manual,
    });
    let (adapter, seen) = scripted_adapter(
        vec![("web_search", r#"{"queries":["rust","tokio"]}"#.to_string())],
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
    let schema = all_schemas
        .iter()
        .flatten()
        .find(|schema| schema.name == "web_search")
        .expect("model must see web_search");
    let properties = schema
        .parameters
        .get("properties")
        .expect("schema parameters must carry properties");
    assert!(properties.get("queries").is_some());
    assert!(properties.get("query").is_some());
    assert!(events.iter().any(|event| matches!(
        event,
        UiEvent::ToolResult { ok: true, output, .. }
            if output.contains("[Rust](https://rust-lang.org)")
                && output.contains("[Tokio](https://tokio.rs)")
    )));
    execution.dispose();
}
