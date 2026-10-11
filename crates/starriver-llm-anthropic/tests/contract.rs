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
