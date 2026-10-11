//! LLP §3.3 contract suite for the OpenAI-completions adapter: the five
//! invariants (finish-is-last, close-at-finish, cancel→ABORTED,
//! idle→LLM_STREAM_IDLE_TIMEOUT, error normalization) driven through a local
//! mock HTTP server.

use std::sync::Arc;

use cordis_rslite::{Context, EventBus, PluginRegistry};
use futures::StreamExt;
use starriver_kernel::llm::{
    ContentBlock, GenerateOptions, LlmAdapter, Message, MessageId, MessageRole, MessageSource,
    StreamChunk,
};
use starriver_llm::contract_tests::{
    assert_finish_is_last, assert_open_blocks_are_closed, terminal_error_code, text_blocks,
};
use starriver_llm::{in_memory_llm_plugin, InMemoryLlmRuntime};
use starriver_llm_openai::{
    CredentialRef, OpenAiCompletionsAdapter, OpenAiCompletionsPlugin, ProviderProfile,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

fn options() -> GenerateOptions {
    GenerateOptions {
        provider: "deepseek".into(),
        model: "deepseek-v4-flash".into(),
        reasoning_effort: None,
        messages: vec![Message {
            id: MessageId("m1".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::Text("hi".into())],
            source: MessageSource::User,
        }],
        system: None,
        tools: None,
        temperature: None,
        max_tokens: None,
        stop: None,
        session_id: None,
        agent_id: None,
        tool_choice: None,
        cancellation: None,
        purpose: None,
        loop_assembled: false,
    }
}

fn adapter(port: u16, idle_ms: u64) -> OpenAiCompletionsAdapter {
    OpenAiCompletionsAdapter::new(
        "deepseek".into(),
        Arc::new(move || ProviderProfile {
            provider: "deepseek".into(),
            api: "openai-completions".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            credential: Some(CredentialRef::Literal("test-key".into())),
            models: Vec::new(),
            retry_policy: None,
            idle_timeout_ms: idle_ms,
            max_tokens: None,
            thinking: None,
            reasoning_effort: None,
        }),
    )
    .with_idle_timeout(idle_ms)
}

fn responses_adapter(port: u16, idle_ms: u64) -> OpenAiCompletionsAdapter {
    OpenAiCompletionsAdapter::new(
        "openai".into(),
        Arc::new(move || ProviderProfile {
            provider: "openai".into(),
            api: "openai-responses".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            credential: Some(CredentialRef::Literal("test-key".into())),
            models: Vec::new(),
            retry_policy: None,
            idle_timeout_ms: idle_ms,
            max_tokens: None,
            thinking: None,
            reasoning_effort: None,
        }),
    )
    .with_idle_timeout(idle_ms)
}

/// Serve exactly one HTTP response (status line, extra headers, body) and
/// return the bound port.
async fn serve_once(status: String, headers: String, body: String) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let response = format!("{status}\r\n{headers}\r\n{body}");
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.flush().await;
    });
    port
}

/// Serve headers, write `first`, then hold the connection open forever
/// (for cancel/idle tests).
async fn serve_then_stall(first: String, idle_ms: u64) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(first.as_bytes()).await;
        let _ = socket.flush().await;
        // Keep the connection open beyond any idle timeout.
        tokio::time::sleep(std::time::Duration::from_millis(idle_ms * 3)).await;
    });
    port
}

/// Serve headers + `part1`, then after a delay write `part2` and close.
async fn serve_two_phase(part1: String, delay_ms: u64, part2: String) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(part1.as_bytes()).await;
        let _ = socket.flush().await;
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        let _ = socket.write_all(part2.as_bytes()).await;
        let _ = socket.flush().await;
    });
    port
}

const HAPPY_SSE: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\" think\"},\"index\":0}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"index\":0}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]},\"index\":0}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}}]},\"index\":0}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\",\"index\":0,\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}]}\n\n",
    "data: [DONE]\n\n",
);

const HAPPY_RESPONSES_SSE: &str = concat!(
    "event: response.output_item.added\n",
    "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\"}}\n\n",
    "event: response.reasoning_summary_text.delta\n",
    "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\" think\"}\n\n",
    "event: response.output_item.done\n",
    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"think\"}]}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n",
    "event: response.output_text.done\n",
    "data: {\"type\":\"response.output_text.done\",\"text\":\"Hello\"}\n\n",
    "event: response.output_item.added\n",
    "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"bash\"}}\n\n",
    "event: response.function_call_arguments.delta\n",
    "data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}\n\n",
    "event: response.output_item.done\n",
    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"bash\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"output_tokens_details\":{\"reasoning_tokens\":3}}}}\n\n",
);

#[tokio::test]
async fn happy_path_finish_is_last_and_blocks_close() {
    let port = serve_once(
        "HTTP/1.1 200 OK".into(),
        "content-type: text/event-stream\r\n".into(),
        HAPPY_SSE.into(),
    )
    .await;
    let chunks: Vec<_> = adapter(port, 5_000).stream(options()).collect().await;

    assert_finish_is_last(&chunks);
    assert_open_blocks_are_closed(&chunks);
    assert_eq!(text_blocks(&chunks), vec!["Hello".to_string()]);
    assert!(
        chunks.iter().any(|c| matches!(
            c,
            Ok(StreamChunk::BlockEnd {
                block: ContentBlock::ToolCall { id, name, .. },
                ..
            }) if id.0 == "call_1" && name.as_deref() == Some("bash")
        )),
        "tool call must be delivered as a closed block"
    );
    assert!(
        chunks.iter().any(|c| matches!(
            c,
            Ok(StreamChunk::BlockEnd {
                block: ContentBlock::Reasoning(t),
                ..
            }) if t == " think"
        )),
        "reasoning block must be delivered as a closed block"
    );
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, Ok(StreamChunk::Usage { .. }))),
        "usage must surface from the tail chunk"
    );
}

#[tokio::test]
async fn missing_credential_surfaces_miss_credential_before_any_http() {
    // No credential at all → MISSING_CREDENTIAL, no HTTP round-trip needed.
    let no_cred = OpenAiCompletionsAdapter::new(
        "deepseek".into(),
        Arc::new(|| ProviderProfile {
            provider: "deepseek".into(),
            api: "openai-completions".into(),
            base_url: "http://127.0.0.1:1".into(),
            credential: None,
            models: Vec::new(),
            retry_policy: None,
            idle_timeout_ms: 5_000,
            max_tokens: None,
            thinking: None,
            reasoning_effort: None,
        }),
    );
    let chunks: Vec<_> = no_cred.stream(options()).collect().await;
    assert_eq!(
        terminal_error_code(&chunks),
        Some("MISSING_CREDENTIAL".to_string()),
        "no credential must fail with MISSING_CREDENTIAL"
    );

    // A credential ref pointing at a missing file → MISSING_CREDENTIAL too.
    let missing_file = OpenAiCompletionsAdapter::new(
        "deepseek".into(),
        Arc::new(|| ProviderProfile {
            provider: "deepseek".into(),
            api: "openai-completions".into(),
            base_url: "http://127.0.0.1:1".into(),
            credential: Some(CredentialRef::File(
                "/nonexistent/starriver-credential-xyz".into(),
            )),
            models: Vec::new(),
            retry_policy: None,
            idle_timeout_ms: 5_000,
            max_tokens: None,
            thinking: None,
            reasoning_effort: None,
        }),
    );
    let chunks: Vec<_> = missing_file.stream(options()).collect().await;
    assert_eq!(
        terminal_error_code(&chunks),
        Some("MISSING_CREDENTIAL".to_string()),
        "a broken credential ref must fail with MISSING_CREDENTIAL"
    );
}

#[tokio::test]
async fn caller_cancellation_surfaces_aborted() {
    let port = serve_then_stall(
        "data: {\"choices\":[{\"delta\":{\"content\":\"He\"},\"index\":0}]}\n\n".into(),
        5_000,
    )
    .await;
    let cancel = CancellationToken::new();
    let mut opts = options();
    opts.cancellation = Some(cancel.clone());
    let mut stream = adapter(port, 5_000).stream(opts);

    let first = stream.next().await;
    assert!(first.is_some(), "server chunk must arrive before cancel");
    cancel.cancel();
    let rest: Vec<_> = stream.collect().await;
    assert_eq!(
        terminal_error_code(&rest).as_deref(),
        Some("ABORTED"),
        "cancellation must surface as the terminal ABORTED error"
    );
}

#[tokio::test]
async fn stalled_stream_surfaces_idle_timeout() {
    let port = serve_then_stall(String::new(), 300).await;
    let chunks: Vec<_> = adapter(port, 300).stream(options()).collect().await;
    assert_eq!(
        terminal_error_code(&chunks).as_deref(),
        Some("LLM_STREAM_IDLE_TIMEOUT"),
        "a stalled stream must surface LLM_STREAM_IDLE_TIMEOUT"
    );
}

#[tokio::test]
async fn http_errors_normalize_with_retry_after_and_request_id() {
    let port = serve_once(
        "HTTP/1.1 429 Too Many Requests".into(),
        "retry-after: 7\r\nx-request-id: req_abc\r\n".into(),
        "rate limited".into(),
    )
    .await;
    let chunks: Vec<_> = adapter(port, 5_000).stream(options()).collect().await;
    let err = chunks.last().unwrap().as_ref().expect_err("429 must error");
    assert_eq!(err.code(), "RATE_LIMIT");
    let failure = err.to_failure();
    assert_eq!(failure.provider_retry_after_ms, Some(7_000));
    assert_eq!(failure.request_id.as_deref(), Some("req_abc"));

    let port = serve_once(
        "HTTP/1.1 400 Bad Request".into(),
        "content-type: application/json\r\n".into(),
        r#"{"error":{"message":"maximum context length exceeded"}}"#.into(),
    )
    .await;
    let chunks: Vec<_> = adapter(port, 5_000).stream(options()).collect().await;
    assert_eq!(
        chunks
            .last()
            .unwrap()
            .as_ref()
            .expect_err("400 must error")
            .code(),
        "CONTEXT_WINDOW_EXCEEDED"
    );

    let port = serve_once(
        "HTTP/1.1 500 Internal Server Error".into(),
        String::new(),
        "boom".into(),
    )
    .await;
    let chunks: Vec<_> = adapter(port, 5_000).stream(options()).collect().await;
    assert_eq!(
        chunks
            .last()
            .unwrap()
            .as_ref()
            .expect_err("500 must error")
            .code(),
        "SERVER"
    );
}

#[tokio::test]
async fn plugin_mounts_routes_into_the_llm_runtime() {
    let port = serve_once(
        "HTTP/1.1 200 OK".into(),
        "content-type: text/event-stream\r\n".into(),
        HAPPY_SSE.into(),
    )
    .await;
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    registry.register(in_memory_llm_plugin());
    registry.register(OpenAiCompletionsPlugin::new(Arc::new(move || {
        vec![ProviderProfile {
            provider: "deepseek".into(),
            api: "openai-completions".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            credential: Some(CredentialRef::Literal("test-key".into())),
            models: Vec::new(),
            retry_policy: None,
            idle_timeout_ms: 5_000,
            max_tokens: None,
            thinking: None,
            reasoning_effort: None,
        }]
    })));
    registry.boot(&ctx, &bus).await.expect("plugins boot");

    let llm = ctx.require::<InMemoryLlmRuntime>().expect("llm runtime");
    assert_eq!(
        llm.registered_providers(),
        vec!["deepseek".to_string()],
        "plugin-mounted route must be registered under its declared provider"
    );
}

#[tokio::test]
async fn plugin_conflict_fails_the_plugin_activation() {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    registry.register(in_memory_llm_plugin());
    for _ in 0..2 {
        registry.register(OpenAiCompletionsPlugin::new(Arc::new(|| {
            vec![ProviderProfile {
                provider: "deepseek".into(),
                api: "openai-completions".into(),
                base_url: "http://127.0.0.1:1".into(),
                credential: None,
                models: Vec::new(),
                retry_policy: None,
                idle_timeout_ms: 5_000,
                max_tokens: None,
                thinking: None,
                reasoning_effort: None,
            }]
        })));
    }

    let err = registry
        .boot(&ctx, &bus)
        .await
        .err()
        .expect("conflict must fail the boot");
    assert!(
        err.to_string().contains("register `deepseek`"),
        "conflict must name the failing route: {err}"
    );
}

#[tokio::test]
async fn responses_protocol_streams_blocks_reasoning_and_usage() {
    let port = serve_once(
        "HTTP/1.1 200 OK".into(),
        "content-type: text/event-stream\r\n".into(),
        HAPPY_RESPONSES_SSE.into(),
    )
    .await;
    let chunks: Vec<_> = responses_adapter(port, 5_000)
        .stream(options())
        .collect()
        .await;

    assert_finish_is_last(&chunks);
    assert_open_blocks_are_closed(&chunks);
    assert_eq!(text_blocks(&chunks), vec!["Hello".to_string()]);
    assert!(
        chunks.iter().any(|c| matches!(
            c,
            Ok(StreamChunk::BlockEnd {
                block: ContentBlock::Custom { kind, .. },
                ..
            }) if kind == starriver_llm_openai::responses::REASONING_ITEM_KIND
        )),
        "the raw reasoning item must be preserved for passback"
    );
    assert!(
        chunks.iter().any(|c| matches!(
            c,
            Ok(StreamChunk::BlockEnd {
                block: ContentBlock::ToolCall { id, name, .. },
                ..
            }) if id.0 == "call_1" && name.as_deref() == Some("bash")
        )),
        "function call must be delivered as a closed block"
    );
    assert!(
        chunks.iter().any(
            |c| matches!(c, Ok(StreamChunk::Usage { usage }) if usage.reasoning_tokens == Some(3))
        ),
        "usage must surface from response.completed"
    );
}

#[tokio::test]
async fn same_generation_freeze_keeps_in_flight_stream_on_original_snapshot() {
    use std::sync::Mutex;

    // Generation 1 endpoint: two-phase mock that completes with "gen1".
    let port = serve_two_phase(
        "data: {\"choices\":[{\"delta\":{\"content\":\"gen1\"},\"index\":0}]}\n\n".into(),
        400,
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\",\"index\":0,\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}]}\n\ndata: [DONE]\n\n"
            .into(),
    )
    .await;
    let shared: Arc<Mutex<ProviderProfile>> = Arc::new(Mutex::new(ProviderProfile {
        provider: "freeze".into(),
        api: "openai-completions".into(),
        base_url: format!("http://127.0.0.1:{port}"),
        credential: Some(CredentialRef::Literal("key-gen1".into())),
        models: Vec::new(),
        retry_policy: None,
        idle_timeout_ms: 5_000,
        max_tokens: None,
        thinking: None,
        reasoning_effort: None,
    }));
    let shared_for_thunk = Arc::clone(&shared);
    let adapter = OpenAiCompletionsAdapter::new(
        "freeze".into(),
        Arc::new(move || shared_for_thunk.lock().unwrap().clone()),
    );

    // Start the stream: the endpoint+credential snapshot is captured once.
    let mut stream = adapter.stream(options());
    let first = stream.next().await;
    assert!(first.is_some(), "first chunk must come from generation 1");

    // Reconfigure mid-stream: point the route at an unreachable endpoint.
    shared.lock().unwrap().base_url = "http://127.0.0.1:1".into();
    shared.lock().unwrap().credential = Some(CredentialRef::Literal("key-gen2".into()));

    // The in-flight stream must finish on the ORIGINAL snapshot.
    let rest: Vec<_> = stream.collect().await;
    assert_finish_is_last(&rest);
    assert_eq!(
        text_blocks(&rest),
        vec!["gen1".to_string()],
        "in-flight stream must ride generation 1"
    );

    // A NEW stream picks up generation 2 (unreachable → transport error).
    let chunks: Vec<_> = adapter.stream(options()).collect().await;
    assert_eq!(
        terminal_error_code(&chunks).as_deref(),
        Some("TRANSPORT"),
        "the next stream must ride the new snapshot"
    );
}

// ── Round-5 regressions: byte-stream framing ──

/// Byte-level two-phase writer: `String` cannot carry a lone partial
/// UTF-8 byte, so the split-chunk regression writes raw bytes.
async fn serve_two_phase_bytes(part1: Vec<u8>, delay_ms: u64, part2: Vec<u8>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(&part1).await;
        let _ = socket.flush().await;
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        let _ = socket.write_all(&part2).await;
        let _ = socket.flush().await;
    });
    port
}

/// BUG-46: network chunks may split a multi-byte UTF-8 character in half.
/// Decoding each chunk independently replaces both halves with U+FFFD — the
/// text is permanently corrupted. Line-buffered decoding must survive the
/// split (a `\n` byte can never appear inside a multi-byte char).
#[tokio::test]
async fn multibyte_delta_split_across_chunks_survives() {
    // a three-byte CJK char (U+4F60) = E4 BD A0; part1 cuts after the first byte.
    let port = serve_two_phase_bytes(
        b"data: {\"choices\":[{\"delta\":{\"content\":\"\xe4".to_vec(),
        30,
        b"\xbd\xa0\"},\"index\":0}]}\n\ndata: [DONE]\n\n".to_vec(),
    )
    .await;
    let chunks: Vec<_> = adapter(port, 5_000).stream(options()).collect().await;
    assert_finish_is_last(&chunks);
    let texts = text_blocks(&chunks);
    assert_eq!(
        texts,
        vec!["\u{4f60}".to_string()],
        "a character split across TCP chunks must reassemble (got {texts:?})"
    );
}

/// BUG-55: the SSE spec allows `data:` without the trailing space (strip at
/// most ONE leading space from the field value). Streams using the compact
/// form must not be swallowed whole.
#[tokio::test]
async fn data_prefix_without_space_still_delivers() {
    let body = concat!(
        "data:{\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"index\":0}]}\n\n",
        "data: [DONE]\n\n",
    );
    let port = serve_once(
        "HTTP/1.1 200 OK".into(),
        "content-type: text/event-stream\r\n".into(),
        body.into(),
    )
    .await;
    let chunks: Vec<_> = adapter(port, 5_000).stream(options()).collect().await;
    assert_finish_is_last(&chunks);
    assert_eq!(
        text_blocks(&chunks),
        vec!["Hello".to_string()],
        "compact `data:` frames must parse like `data: ` frames"
    );
}
