//! LLP §3.3 contract suite for the Anthropic Messages adapter: the five
//! invariants driven through a local mock HTTP server.

use futures::StreamExt;
use starriver_kernel::llm::{
    ContentBlock, GenerateOptions, LlmAdapter, Message, MessageId, MessageRole, MessageSource,
    StreamChunk,
};
use starriver_llm::contract_tests::{
    assert_finish_is_last, assert_open_blocks_are_closed, terminal_error_code, text_blocks,
};
use starriver_llm_anthropic::AnthropicAdapter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

fn options() -> GenerateOptions {
    GenerateOptions {
        provider: "claude".into(),
        model: "claude-3-5-sonnet".into(),
        reasoning_effort: None,
        messages: vec![Message {
            id: MessageId("m1".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::Text("hi".into())],
            source: MessageSource::user(),
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

fn adapter(port: u16, idle_ms: u64) -> AnthropicAdapter {
    AnthropicAdapter::new(
        "claude".into(),
        format!("http://127.0.0.1:{port}"),
        Some(starriver_llm_anthropic::CredentialRef::Literal(
            "test-key".into(),
        )),
    )
    .with_idle_timeout(idle_ms)
}

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
        tokio::time::sleep(std::time::Duration::from_millis(idle_ms * 3)).await;
    });
    port
}

const HAPPY_SSE: &str = concat!(
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
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
        chunks
            .iter()
            .any(|c| matches!(c, Ok(StreamChunk::Usage { .. }))),
        "usage must surface from message_delta"
    );
}

/// BUG-40 regression: the SSE spec allows `\r\n` line endings (frame
/// separator `\r\n\r\n`). The frame splitter only looked for `\n\n`, so a
/// CRLF server's frames never parsed and the stream ended with no text.
#[tokio::test]
async fn crlf_line_endings_still_deliver_frames() {
    let crlf = HAPPY_SSE.replace('\n', "\r\n");
    let port = serve_once(
        "HTTP/1.1 200 OK".into(),
        "content-type: text/event-stream\r\n".into(),
        crlf,
    )
    .await;
    let chunks: Vec<_> = adapter(port, 5_000).stream(options()).collect().await;
    assert_finish_is_last(&chunks);
    assert_eq!(
        text_blocks(&chunks),
        vec!["Hello".to_string()],
        "CRLF-framed SSE must parse identically to LF"
    );
}

#[tokio::test]
async fn caller_cancellation_surfaces_aborted() {
    let port = serve_then_stall(
        "event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n"
            .into(),
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

/// The cancel must also interrupt a stream that has ALREADY gone silent: the
/// old loop-top poll only ran between chunk yields, so a Stop pressed while
/// the endpoint delivered no bytes waited out the whole idle window
/// (2026-11-01 incident). Drive the stream back into its wait, then cancel.
#[tokio::test]
async fn cancellation_during_silence_surfaces_aborted_promptly() {
    let port = serve_then_stall(
        "event: content_block_start\n\
         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n"
            .into(),
        3_000,
    )
    .await;
    let cancel = CancellationToken::new();
    let mut opts = options();
    opts.cancellation = Some(cancel.clone());
    let mut stream = adapter(port, 1_000).stream(opts);

    let first = stream.next().await;
    assert!(first.is_some(), "server chunk must arrive before cancel");
    // Poll once more: no second chunk is coming, so this parks the stream
    // inside its request wait — exactly where the old loop-top poll could
    // not observe a cancellation.
    let parked = tokio::time::timeout(std::time::Duration::from_millis(150), stream.next()).await;
    assert!(parked.is_err(), "stream must be parked with no bytes coming");
    cancel.cancel();
    let started = std::time::Instant::now();
    let rest: Vec<_> = stream.collect().await;
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "cancel during silence must abort promptly, took {:?}",
        started.elapsed()
    );
    assert_eq!(
        terminal_error_code(&rest).as_deref(),
        Some("ABORTED"),
        "cancellation during silence must surface ABORTED, not the idle timeout"
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
async fn http_errors_normalize() {
    let port = serve_once(
        "HTTP/1.1 429 Too Many Requests".into(),
        String::new(),
        "rate limited".into(),
    )
    .await;
    let chunks: Vec<_> = adapter(port, 5_000).stream(options()).collect().await;
    assert_eq!(
        chunks
            .last()
            .unwrap()
            .as_ref()
            .expect_err("429 must error")
            .code(),
        "RATE_LIMIT"
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

/// BUG-167 regression: a 429 must carry the provider-advised Retry-After
/// window and the request id off the response headers.
#[tokio::test]
async fn rate_limit_carries_retry_after_and_request_id() {
    let port = serve_once(
        "HTTP/1.1 429 Too Many Requests".into(),
        // serve_once splices a single \r\n between headers and body — a
        // non-empty header block must terminate its last line itself.
        "retry-after: 7\r\nrequest-id: req_abc123\r\n".into(),
        r#"{"type":"error","error":{"type":"rate_limit_error"}}"#.into(),
    )
    .await;
    let chunks: Vec<_> = adapter(port, 5_000).stream(options()).collect().await;
    let error = chunks
        .last()
        .unwrap()
        .as_ref()
        .expect_err("429 must error");
    assert_eq!(error.code(), "RATE_LIMIT");
    match error {
        starriver_kernel::llm::LlmError::Call {
            provider_retry_after_ms,
            request_id,
            ..
        } => {
            assert_eq!(*provider_retry_after_ms, Some(7_000));
            assert_eq!(request_id.as_deref(), Some("req_abc123"));
        }
        other => panic!("expected Call error, got {other:?}"),
    }
}

/// BUG-162 regression: a documented mid-stream `error` SSE event must
/// surface as its provider error class, not a bare stream truncation.
#[tokio::test]
async fn mid_stream_error_event_surfaces_server_error() {
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-x\",\"usage\":{\"input_tokens\":3}}}\n\n",
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    );
    let port = serve_once(
        "HTTP/1.1 200 OK".into(),
        "content-type: text/event-stream\r\n".into(),
        sse.into(),
    )
    .await;
    let chunks: Vec<_> = adapter(port, 5_000).stream(options()).collect().await;
    let error = chunks
        .last()
        .unwrap()
        .as_ref()
        .expect_err("error event must fail the stream");
    assert_eq!(error.code(), "SERVER");
    assert!(
        error.to_string().contains("overloaded_error"),
        "the provider error type must survive to the caller"
    );
}

/// BUG-169 regression: an OpenAI-style base_url carrying `/v1` must hit
/// `/v1/messages` exactly once, not `/v1/v1/messages`.
#[tokio::test]
async fn base_url_with_v1_suffix_hits_messages_once() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let request_line = String::from_utf8_lossy(&buf)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        let _ = tx.send(request_line);
        let response = concat!(
            "HTTP/1.1 200 OK\r\n",
            "content-type: text/event-stream\r\n\r\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.flush().await;
    });
    let adapter = AnthropicAdapter::new(
        "claude".into(),
        format!("http://127.0.0.1:{port}/v1"),
        Some(starriver_llm_anthropic::CredentialRef::Literal("k".into())),
    )
    .with_idle_timeout(5_000);
    let chunks: Vec<_> = adapter.stream(options()).collect().await;
    let request_line = rx.await.expect("request line captured");
    assert!(
        request_line.contains("POST /v1/messages "),
        "base with /v1 must hit /v1/messages exactly once, got `{request_line}`"
    );
    assert!(
        chunks.iter().any(|c| matches!(c, Ok(StreamChunk::Finish { .. }))),
        "the stream itself must complete"
    );
}
