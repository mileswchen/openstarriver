//! LLP §3.3 contract suite for the Ollama adapter: the five invariants
//! driven through a local mock HTTP server.

use futures::StreamExt;
use starriver_kernel::llm::{
    ContentBlock, GenerateOptions, LlmAdapter, Message, MessageId, MessageRole, MessageSource,
};
use starriver_llm::contract_tests::{
    assert_finish_is_last, assert_open_blocks_are_closed, terminal_error_code, text_blocks,
};
use starriver_llm_ollama::OllamaAdapter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

fn options() -> GenerateOptions {
    GenerateOptions {
        provider: "ollama".into(),
        model: "qwen3".into(),
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

fn adapter(port: u16, idle_ms: u64) -> OllamaAdapter {
    OllamaAdapter::new("ollama".into(), format!("http://127.0.0.1:{port}"))
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
        let head = "HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\n\r\n";
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(first.as_bytes()).await;
        let _ = socket.flush().await;
        tokio::time::sleep(std::time::Duration::from_millis(idle_ms * 3)).await;
    });
    port
}

const HAPPY_NDJSON: &str = concat!(
    "{\"model\":\"qwen3\",\"message\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"done\":false}\n",
    "{\"model\":\"qwen3\",\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true}\n",
);

#[tokio::test]
async fn happy_path_finish_is_last_and_blocks_close() {
    let port = serve_once(
        "HTTP/1.1 200 OK".into(),
        "content-type: application/x-ndjson\r\n".into(),
        HAPPY_NDJSON.into(),
    )
    .await;
    let chunks: Vec<_> = adapter(port, 5_000).stream(options()).collect().await;

    assert_finish_is_last(&chunks);
    assert_open_blocks_are_closed(&chunks);
    assert_eq!(text_blocks(&chunks), vec!["Hello".to_string()]);
}

#[tokio::test]
async fn caller_cancellation_surfaces_aborted() {
    let port = serve_then_stall(
        "{\"model\":\"qwen3\",\"message\":{\"role\":\"assistant\",\"content\":\"He\"},\"done\":false}\n"
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
        "HTTP/1.1 404 Not Found".into(),
        String::new(),
        "model not found".into(),
    )
    .await;
    let chunks: Vec<_> = adapter(port, 5_000).stream(options()).collect().await;
    assert_eq!(
        chunks
            .last()
            .unwrap()
            .as_ref()
            .expect_err("404 must error")
            .code(),
        "MODEL_NOT_FOUND"
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
