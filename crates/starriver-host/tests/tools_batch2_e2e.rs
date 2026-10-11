//! Batch-2 tool e2e: glob/web_fetch/web_search/lsp/patch through the plugin
//! kernel with scripted mock models.

pub use std::sync::Arc;
use std::sync::Mutex;

use cordis_rslite::{Context, EventBus};
use starriver_host::plugin_kernel::{PluginKernel, WebToolOverrides};
use starriver_kernel::llm::{
    ContentBlock, FinishReason, GenerateOptions, LlmAdapter, LlmError, StreamChunk,
};
use starriver_kernel::ui::UiEvent;
use starriver_session_persistence::SqliteSessionPersistence;

/// Scripted adapter plus the tool names it sent to the model.
type Scripted = (Arc<dyn LlmAdapter>, Arc<Mutex<Vec<Vec<String>>>>);

/// Adapter that emits scripted (tool name, args JSON) calls in order, then a
/// closing text block.
fn tool_call_script(calls: Vec<(&'static str, String)>, final_text: &'static str) -> Scripted {
    struct ScriptedCalls {
        calls: std::sync::Mutex<std::collections::VecDeque<(&'static str, String)>>,
        final_text: &'static str,
        tools: Arc<Mutex<Vec<Vec<String>>>>,
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
            let tools = options
                .tools
                .as_ref()
                .map(|tools| tools.iter().map(|t| t.name.clone()).collect())
                .unwrap_or_default();
            self.tools.lock().unwrap().push(tools);
            let call = self.calls.lock().unwrap().pop_front();
            match call {
                Some((name, args_json)) => Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::ToolCallDelta {
                        index: 0,
                        id: starriver_kernel::llm::CallId("c1".into()),
                        name: Some(name.into()),
                        arguments_delta: args_json.clone(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::ToolCall {
                            id: starriver_kernel::llm::CallId("c1".into()),
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
    let tools = Arc::new(Mutex::new(Vec::new()));
    let adapter = Arc::new(ScriptedCalls {
        calls: std::sync::Mutex::new(calls.into()),
        final_text,
        tools: Arc::clone(&tools),
    }) as Arc<dyn LlmAdapter>;
    (adapter, tools)
}

fn kernel_with_adapter(
    path: std::path::PathBuf,
    adapter: Arc<dyn LlmAdapter>,
) -> (PluginKernel, EventBus) {
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(SqliteSessionPersistence::open_with_bus(path, bus.clone()).unwrap());
    let scopes = Arc::new(starriver_kernel::agent::AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(ctx, bus.clone(), persistence, scopes);
    kernel.set_llm_adapter(adapter);
    (kernel, bus)
}

fn kernel_with_web_overrides(
    path: std::path::PathBuf,
    adapter: Arc<dyn LlmAdapter>,
    overrides: WebToolOverrides,
) -> PluginKernel {
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(SqliteSessionPersistence::open_with_bus(path, bus.clone()).unwrap());
    let scopes = Arc::new(starriver_kernel::agent::AgentScopeRegistry::new());
    let kernel = PluginKernel::with_services_and_web_overrides(
        ctx,
        bus.clone(),
        persistence,
        scopes,
        overrides,
    );
    kernel.set_llm_adapter(adapter);
    kernel
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

async fn run(
    kernel: &PluginKernel,
) -> (Vec<UiEvent>, starriver_host::plugin_kernel::KernelExecution) {
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
    (seen, execution)
}

#[tokio::test]
async fn glob_read_patch_end_to_end() {
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::AgentScopeRegistry;
    use starriver_kernel::sandbox::SandboxMode;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("hello.txt");
    std::fs::write(&target, "hello\n").unwrap();
    let (adapter, _t) =
        tool_call_script(
            vec![
            (
                "glob",
                serde_json::json!({ "pattern": "hello.txt", "path": dir.path().to_string_lossy() })
                    .to_string(),
            ),
            ("read", serde_json::json!({ "path": target.to_string_lossy() }).to_string()),
            (
                "patch",
                serde_json::json!({
                    "path": target.to_string_lossy(),
                    "diff": "@@ -1 +1 @@\n-hello\n+hello world\n"
                })
                .to_string(),
            ),
        ],
            "done",
        );
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("s.db"), bus.clone()).unwrap(),
    );
    let scopes = Arc::new(AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(ctx, bus.clone(), persistence, scopes);
    kernel.set_preset(starriver_host::preset::Preset {
        sandbox: SandboxMode::DangerFullAccess,
        approval: starriver_agent_loop::approval::ApprovalMode::Manual,
    });
    kernel.set_llm_adapter(adapter);
    let (seen, execution) = run(&kernel).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: true, .. })));
    assert!(seen.iter().any(|e| matches!(e, UiEvent::TurnEnd { .. })));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello world\n");
    execution.dispose();
}

#[tokio::test]
async fn web_fetch_ssrf_blocked_in_loop() {
    let dir = tempfile::tempdir().unwrap();
    let (adapter, _t) = tool_call_script(
        vec![(
            "web_fetch",
            serde_json::json!({ "url": "http://127.0.0.1:8080/x" }).to_string(),
        )],
        "done",
    );
    let (kernel, _bus) = kernel_with_adapter(dir.path().join("s.db"), adapter);
    let (seen, execution) = run(&kernel).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: false, .. })));
    execution.dispose();
}

#[tokio::test]
async fn web_fetch_and_search_hit_local_mocks() {
    use starriver_host::tools::web_common::test_support::{http_ok, spawn_http_server};
    use starriver_host::tools::web_fetch::{web_fetch_definition_with, WebFetch};
    use starriver_host::tools::web_search::{web_search_definition_with, WebSearch};

    let ddg_html = r#"<a class="result__a" href="//x/l/?uddg=https%3A%2F%2Frust-lang.org">Rust</a><a class="result__snippet">A safe language</a>"#;
    let server = spawn_http_server(vec![http_ok("landed body"), http_ok(ddg_html)]).await;
    let fetch = web_fetch_definition_with(WebFetch::with_guard(Arc::new(|_url: &str| {
        Box::pin(async { Ok::<(), String>(()) })
    })));
    let search = web_search_definition_with(WebSearch::with_endpoints(
        format!("{server}/ddg"),
        format!("{server}/bing"),
    ));
    let dir = tempfile::tempdir().unwrap();
    let (adapter, _t) = tool_call_script(
        vec![
            (
                "web_fetch",
                serde_json::json!({ "url": format!("{server}/page") }).to_string(),
            ),
            (
                "web_search",
                serde_json::json!({ "query": "rust" }).to_string(),
            ),
        ],
        "done",
    );
    let kernel = kernel_with_web_overrides(
        dir.path().join("s.db"),
        adapter,
        WebToolOverrides {
            web_fetch: Some(fetch),
            web_search: Some(search),
        },
    );
    let (seen, execution) = run(&kernel).await;
    let ok_results = seen
        .iter()
        .filter(|e| matches!(e, UiEvent::ToolResult { ok: true, .. }))
        .count();
    assert_eq!(ok_results, 2);
    execution.dispose();
}

#[tokio::test]
async fn patch_requires_approval_ask() {
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::AgentScopeRegistry;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a.txt");
    std::fs::write(&target, "x\n").unwrap();
    let (adapter, _t) = tool_call_script(
        vec![(
            "patch",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "diff": "@@ -1 +1 @@\n-x\n+y\n"
            })
            .to_string(),
        )],
        "done",
    );
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("s.db"), bus.clone()).unwrap(),
    );
    let scopes = Arc::new(AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(ctx, bus.clone(), persistence, scopes);
    kernel.set_llm_adapter(adapter);
    let kernel = Arc::new(kernel);
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let rx = tx.subscribe();
    let sid = "p1".to_string();
    let kernel_approver = Arc::clone(&kernel);
    let approver = tokio::spawn(async move {
        for _ in 0..500 {
            if kernel_approver.approval_bridge.approve("p1", true, false) {
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
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "y\n");
    execution.dispose();
}

#[tokio::test]
async fn lsp_symbols_end_to_end_via_fake_server() {
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::AgentScopeRegistry;

    static LSP_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = LSP_ENV_LOCK.lock().await;
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fake_rust_analyzer.py");
    std::env::set_var("STARRIVER_RUST_ANALYZER", &fixture);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("proj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    let main = root.join("src/main.rs");
    std::fs::write(&main, "fn main() {}\n").unwrap();
    let (adapter, _t) = tool_call_script(
        vec![(
            "lsp",
            serde_json::json!({
                "operation": "symbols",
                "path": main.to_string_lossy()
            })
            .to_string(),
        )],
        "done",
    );
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("s.db"), bus.clone()).unwrap(),
    );
    let scopes = Arc::new(AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(ctx, bus.clone(), persistence, scopes);
    kernel.set_llm_adapter(adapter);
    let (seen, execution) = run(&kernel).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: true, .. })));
    let session = execution.agent().session();
    assert!(session.events().iter().any(
        |e| e.event_type == "tool/result" && format!("{:?}", e.data).contains("main (1:1-4:1)")
    ));
    execution.dispose();
}

#[tokio::test]
async fn lsp_rejects_unsupported_file_and_missing_workspace() {
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::AgentScopeRegistry;

    let dir = tempfile::tempdir().unwrap();
    let txt = dir.path().join("notes.txt");
    std::fs::write(&txt, "x").unwrap();
    let (adapter, _t) = tool_call_script(
        vec![(
            "lsp",
            serde_json::json!({ "operation": "symbols", "path": txt.to_string_lossy() })
                .to_string(),
        )],
        "done",
    );
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("s.db"), bus.clone()).unwrap(),
    );
    let scopes = Arc::new(AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(ctx, bus.clone(), persistence, scopes);
    kernel.set_llm_adapter(adapter);
    let (seen, execution) = run(&kernel).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: false, .. })));
    execution.dispose();
}

fn kernel_with_preset(
    path: std::path::PathBuf,
    adapter: Arc<dyn LlmAdapter>,
    sandbox: starriver_kernel::sandbox::SandboxMode,
) -> PluginKernel {
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(SqliteSessionPersistence::open_with_bus(path, bus.clone()).unwrap());
    let scopes = Arc::new(starriver_kernel::agent::AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(ctx, bus.clone(), persistence, scopes);
    kernel.set_preset(starriver_host::preset::Preset {
        sandbox,
        approval: starriver_agent_loop::approval::ApprovalMode::Manual,
    });
    kernel.set_llm_adapter(adapter);
    kernel
}

#[tokio::test]
async fn patch_prepend_end_to_end() {
    use starriver_kernel::sandbox::SandboxMode;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("notes.txt");
    std::fs::write(&target, "body\n").unwrap();
    let (adapter, _t) = tool_call_script(
        vec![(
            "patch",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "diff": "@@ -0,0 +1,1 @@\n+head\n"
            })
            .to_string(),
        )],
        "done",
    );
    let kernel = kernel_with_preset(
        dir.path().join("s.db"),
        adapter,
        SandboxMode::DangerFullAccess,
    );
    let (seen, execution) = run(&kernel).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: true, .. })));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "head\nbody\n");
    execution.dispose();
}

#[tokio::test]
async fn patch_ambiguous_diff_fails_end_to_end() {
    use starriver_kernel::sandbox::SandboxMode;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a.txt");
    std::fs::write(&target, "x\nx\n").unwrap();
    let (adapter, _t) = tool_call_script(
        vec![(
            "patch",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "diff": "@@ -1 +1 @@\n-x\n+y\n"
            })
            .to_string(),
        )],
        "done",
    );
    let kernel = kernel_with_preset(
        dir.path().join("s.db"),
        adapter,
        SandboxMode::DangerFullAccess,
    );
    let (seen, execution) = run(&kernel).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: false, .. })));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "x\nx\n");
    execution.dispose();
}

#[tokio::test]
async fn web_fetch_mapped_ipv6_blocked_in_loop() {
    let dir = tempfile::tempdir().unwrap();
    let (adapter, _t) = tool_call_script(
        vec![(
            "web_fetch",
            serde_json::json!({ "url": "http://[::ffff:127.0.0.1]/x" }).to_string(),
        )],
        "done",
    );
    let (kernel, _bus) = kernel_with_adapter(dir.path().join("s.db"), adapter);
    let (seen, execution) = run(&kernel).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: false, .. })));
    execution.dispose();
}

#[tokio::test]
async fn web_fetch_credentials_blocked_in_loop() {
    let dir = tempfile::tempdir().unwrap();
    let (adapter, _t) = tool_call_script(
        vec![(
            "web_fetch",
            serde_json::json!({ "url": "http://user:pass@example.com/x" }).to_string(),
        )],
        "done",
    );
    let (kernel, _bus) = kernel_with_adapter(dir.path().join("s.db"), adapter);
    let (seen, execution) = run(&kernel).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: false, .. })));
    execution.dispose();
}

#[tokio::test]
async fn web_fetch_cross_origin_redirect_blocked_in_loop() {
    use starriver_host::tools::web_common::test_support::spawn_http_server;
    use starriver_host::tools::web_fetch::{web_fetch_definition_with, WebFetch};

    let url = spawn_http_server(vec![
        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.2:8080/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .into(),
    ])
    .await;
    let fetch = web_fetch_definition_with(WebFetch::with_guard(Arc::new(|_url: &str| {
        Box::pin(async { Ok::<(), String>(()) })
    })));
    let dir = tempfile::tempdir().unwrap();
    let (adapter, _t) = tool_call_script(
        vec![(
            "web_fetch",
            serde_json::json!({ "url": format!("{url}/start") }).to_string(),
        )],
        "done",
    );
    let kernel = kernel_with_web_overrides(
        dir.path().join("s.db"),
        adapter,
        WebToolOverrides {
            web_fetch: Some(fetch),
            web_search: None,
        },
    );
    let (seen, execution) = run(&kernel).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: false, .. })));
    execution.dispose();
}

#[tokio::test]
async fn web_search_cross_origin_redirect_fails_in_loop() {
    use starriver_host::tools::web_common::test_support::spawn_http_server;
    use starriver_host::tools::web_search::{web_search_definition_with, WebSearch};

    let url = spawn_http_server(vec![
        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.2:8080/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .into(),
    ])
    .await;
    let search = web_search_definition_with(WebSearch::with_endpoints(
        format!("{url}/ddg"),
        format!("{url}/bing"),
    ));
    let dir = tempfile::tempdir().unwrap();
    let (adapter, _t) = tool_call_script(
        vec![(
            "web_search",
            serde_json::json!({ "query": "rust" }).to_string(),
        )],
        "done",
    );
    let kernel = kernel_with_web_overrides(
        dir.path().join("s.db"),
        adapter,
        WebToolOverrides {
            web_fetch: None,
            web_search: Some(search),
        },
    );
    let (seen, execution) = run(&kernel).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: false, .. })));
    execution.dispose();
}

#[tokio::test]
async fn web_search_falls_back_to_bing_in_loop() {
    use starriver_host::tools::web_common::test_support::{http_ok, spawn_http_server};
    use starriver_host::tools::web_search::{web_search_definition_with, WebSearch};

    let ddg = spawn_http_server(vec![
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .into(),
    ])
    .await;
    let bing_html =
        r#"<li class="b_algo"><h2><a href="https://rust-lang.org">Rust</a></h2><p>Lang</p></li>"#;
    let bing = spawn_http_server(vec![http_ok(bing_html)]).await;
    let search = web_search_definition_with(WebSearch::with_endpoints(ddg, bing));
    let dir = tempfile::tempdir().unwrap();
    let (adapter, _t) = tool_call_script(
        vec![(
            "web_search",
            serde_json::json!({ "query": "rust" }).to_string(),
        )],
        "done",
    );
    let kernel = kernel_with_web_overrides(
        dir.path().join("s.db"),
        adapter,
        WebToolOverrides {
            web_fetch: None,
            web_search: Some(search),
        },
    );
    let (seen, execution) = run(&kernel).await;
    assert!(seen
        .iter()
        .any(|e| matches!(e, UiEvent::ToolResult { ok: true, .. })));
    execution.dispose();
}

#[tokio::test]
async fn web_fetch_returns_markdown_in_loop() {
    use starriver_host::tools::web_common::test_support::spawn_http_server;
    use starriver_host::tools::web_fetch::{web_fetch_definition_with, WebFetch};

    let body = "<h1>Loop Title</h1><p>Body <strong>text</strong></p>";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let url = spawn_http_server(vec![resp]).await;
    let fetch = web_fetch_definition_with(WebFetch::with_guard(Arc::new(|_url: &str| {
        Box::pin(async { Ok::<(), String>(()) })
    })));
    let dir = tempfile::tempdir().unwrap();
    let (adapter, _t) = tool_call_script(
        vec![(
            "web_fetch",
            serde_json::json!({ "url": format!("{url}/page") }).to_string(),
        )],
        "done",
    );
    let kernel = kernel_with_web_overrides(
        dir.path().join("s.db"),
        adapter,
        WebToolOverrides {
            web_fetch: Some(fetch),
            web_search: None,
        },
    );
    let (seen, execution) = run(&kernel).await;
    let out = seen
        .iter()
        .find_map(|e| match e {
            UiEvent::ToolResult {
                ok: true, output, ..
            } => Some(output.clone()),
            _ => None,
        })
        .expect("successful web_fetch result");
    assert!(out.contains("# Loop Title"));
    assert!(out.contains("**text**"));
    execution.dispose();
}

#[tokio::test]
async fn web_fetch_decodes_charset_in_loop() {
    use starriver_host::tools::web_common::test_support::spawn_http_server_bytes;
    use starriver_host::tools::web_fetch::{web_fetch_definition_with, WebFetch};

    let gbk = encoding_rs::GBK.encode("标题").0;
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=gbk\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        gbk.len()
    );
    let mut raw = head.into_bytes();
    raw.extend_from_slice(&gbk);
    let url = spawn_http_server_bytes(vec![raw]).await;
    let fetch = web_fetch_definition_with(WebFetch::with_guard(Arc::new(|_url: &str| {
        Box::pin(async { Ok::<(), String>(()) })
    })));
    let dir = tempfile::tempdir().unwrap();
    let (adapter, _t) = tool_call_script(
        vec![(
            "web_fetch",
            serde_json::json!({ "url": format!("{url}/page") }).to_string(),
        )],
        "done",
    );
    let kernel = kernel_with_web_overrides(
        dir.path().join("s.db"),
        adapter,
        WebToolOverrides {
            web_fetch: Some(fetch),
            web_search: None,
        },
    );
    let (seen, execution) = run(&kernel).await;
    let out = seen
        .iter()
        .find_map(|e| match e {
            UiEvent::ToolResult {
                ok: true, output, ..
            } => Some(output.clone()),
            _ => None,
        })
        .expect("successful web_fetch result");
    assert!(out.contains("标题"));
    execution.dispose();
}

#[tokio::test]
async fn web_fetch_truncates_long_page_in_loop() {
    use starriver_host::tools::web_common::test_support::{http_ok, spawn_http_server};
    use starriver_host::tools::web_fetch::{web_fetch_definition_with, WebFetch};

    let body = "z".repeat(200_000);
    let url = spawn_http_server(vec![http_ok(&body)]).await;
    let fetch = web_fetch_definition_with(WebFetch::with_guard(Arc::new(|_url: &str| {
        Box::pin(async { Ok::<(), String>(()) })
    })));
    let dir = tempfile::tempdir().unwrap();
    let (adapter, _t) = tool_call_script(
        vec![(
            "web_fetch",
            serde_json::json!({ "url": format!("{url}/page") }).to_string(),
        )],
        "done",
    );
    let kernel = kernel_with_web_overrides(
        dir.path().join("s.db"),
        adapter,
        WebToolOverrides {
            web_fetch: Some(fetch),
            web_search: None,
        },
    );
    let (seen, execution) = run(&kernel).await;
    let out = seen
        .iter()
        .find_map(|e| match e {
            UiEvent::ToolResult {
                ok: true, output, ..
            } => Some(output.clone()),
            _ => None,
        })
        .expect("successful web_fetch result");
    // The page is over the in-context cap, so the kernel either keeps the
    // renderer's truncation footer or (for oversized outputs) spills the full
    // text to disk and replaces it with a truncation summary — either way the
    // model must learn the page was cut.
    assert!(
        out.contains("truncated"),
        "expected a truncation signal in: {out}"
    );
    execution.dispose();
}

/// Scripted adapter driving the persistent-terminal six-tool lifecycle: start
/// → write → read → kill. The terminal id is extracted from the previous
/// turn's tool result, so the script works against runtime-generated ids.
struct TerminalLifecycleScript {
    turn: Mutex<usize>,
    tools: Arc<Mutex<Vec<Vec<String>>>>,
}

fn terminal_id_from_result(messages: &[starriver_kernel::llm::Message]) -> Option<String> {
    for message in messages.iter().rev() {
        for block in &message.content {
            if let ContentBlock::ToolResult { content, .. } = block {
                let text = content
                    .iter()
                    .filter_map(|inner| match inner {
                        ContentBlock::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                if let Some(id) = text.strip_prefix("terminal started: ") {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

impl LlmAdapter for TerminalLifecycleScript {
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
        let turn = {
            let mut guard = self.turn.lock().unwrap();
            let current = *guard;
            *guard += 1;
            current
        };
        let call: Option<(&'static str, serde_json::Value)> = match turn {
            0 => Some(("terminal_start", serde_json::json!({}))),
            1 | 2 | 3 => {
                let id = terminal_id_from_result(&options.messages)
                    .expect("terminal id must come from the previous tool result");
                match turn {
                    1 => Some((
                        "terminal_write",
                        serde_json::json!({ "id": id, "input": "echo terminal-loop\n" }),
                    )),
                    2 => Some(("terminal_read", serde_json::json!({ "id": id }))),
                    _ => Some(("terminal_kill", serde_json::json!({ "id": id }))),
                }
            }
            _ => None,
        };
        match call {
            Some((name, args)) => {
                let args = args.to_string();
                Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::ToolCallDelta {
                        index: 0,
                        id: starriver_kernel::llm::CallId(format!("c{turn}")),
                        name: Some(name.into()),
                        arguments_delta: args.clone(),
                    }),
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::ToolCall {
                            id: starriver_kernel::llm::CallId(format!("c{turn}")),
                            name: Some(name.into()),
                            arguments: starriver_kernel::json::JsonValue::from_json_str(&args)
                                .unwrap_or_else(|_| {
                                    starriver_kernel::json::JsonValue::String(args.to_string())
                                }),
                        },
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::ToolCalls,
                        replay_state: None,
                    }),
                ]))
            }
            None => {
                let text = "terminal lifecycle done";
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

#[tokio::test]
async fn terminal_six_tool_lifecycle_in_loop() {
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::AgentScopeRegistry;

    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("s.db"), bus.clone()).unwrap(),
    );
    let scopes = Arc::new(AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(ctx, bus.clone(), persistence, scopes);
    let tools = Arc::new(Mutex::new(Vec::new()));
    let adapter = Arc::new(TerminalLifecycleScript {
        turn: Mutex::new(0),
        tools: Arc::clone(&tools),
    }) as Arc<dyn LlmAdapter>;
    kernel.set_llm_adapter(adapter);
    let kernel = Arc::new(kernel);

    // terminal_start needs approval: auto-approve in a loop.
    let kernel_approver = Arc::clone(&kernel);
    let approver = tokio::spawn(async move {
        for _ in 0..500 {
            if kernel_approver.approval_bridge.approve("p1", true, false) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("approval request never reached the bridge");
    });

    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let rx = tx.subscribe();
    let execution = kernel
        .execute(
            "run".into(),
            Some("p1".into()),
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    approver.await.unwrap();
    let seen = drain_until_terminal(rx).await;

    let results: Vec<&UiEvent> = seen
        .iter()
        .filter(|e| matches!(e, UiEvent::ToolResult { .. }))
        .collect();
    assert_eq!(
        results.len(),
        4,
        "start/write/read/kill each produce a result: {seen:?}"
    );
    assert!(
        results
            .iter()
            .all(|e| matches!(e, UiEvent::ToolResult { ok: true, .. })),
        "{seen:?}"
    );
    assert!(
        results.iter().any(|e| matches!(
            e,
            UiEvent::ToolResult { output, .. } if output.contains("terminal-loop")
        )),
        "read result must carry the echoed output: {seen:?}"
    );
    let names: Vec<String> = seen
        .iter()
        .filter_map(|e| match e {
            UiEvent::ToolCall { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    for tool in [
        "terminal_start",
        "terminal_write",
        "terminal_read",
        "terminal_kill",
    ] {
        assert!(names.iter().any(|n| n == tool), "missing {tool}: {names:?}");
    }
    execution.dispose();
}

#[tokio::test]
async fn terminal_tool_invalid_arguments_fail_in_loop() {
    use starriver_host::plugin_kernel::PluginKernel;
    use starriver_kernel::agent::AgentScopeRegistry;

    let dir = tempfile::tempdir().unwrap();
    let bus = EventBus::new();
    let ctx = Context::new().with_event_bus(&bus);
    let persistence = Arc::new(
        SqliteSessionPersistence::open_with_bus(dir.path().join("s.db"), bus.clone()).unwrap(),
    );
    let scopes = Arc::new(AgentScopeRegistry::new());
    let kernel = PluginKernel::with_scopes(ctx, bus.clone(), persistence, scopes);
    let (adapter, _t) = tool_call_script(
        vec![(
            "terminal_write",
            serde_json::json!({ "input": "x" }).to_string(),
        )],
        "done",
    );
    kernel.set_llm_adapter(adapter);
    let kernel = Arc::new(kernel);
    let (tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(1024);
    let rx = tx.subscribe();
    let execution = kernel
        .execute(
            "run".into(),
            Some("p2".into()),
            Some("m1".into()),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let seen = drain_until_terminal(rx).await;
    assert!(
        seen.iter().any(|e| matches!(
            e,
            UiEvent::ToolResult { ok: false, output, .. } if output.contains("missing id")
        )),
        "missing id must fail in-loop: {seen:?}"
    );
    execution.dispose();
}
