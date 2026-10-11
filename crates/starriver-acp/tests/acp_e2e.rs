//! WI-70 ACP server end-to-end: a channel transport drives the full JSON-RPC
//! conversation against a real `HostRuntime` with scripted adapters —
//! handshake, session lifecycle, prompt streaming, images, cancellation,
//! and the permission bridge.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::stream::BoxStream;
use futures::StreamExt as _;
use serde_json::{json, Value};
use starriver_acp::{AcpRead, AcpServer, AcpWrite};
use starriver_host::HostRuntime;
use starriver_kernel::llm::{
    ContentBlock, ContentBlockType, FinishReason, GenerateOptions, LlmAdapter, LlmError,
    StreamChunk,
};
use tokio::sync::mpsc as ampsc;

/// Serialize HOME mutation across tests (HostRuntime reads `~/.starriver`).
static HOME_LOCK: Mutex<()> = Mutex::new(());

fn with_temp_home(f: impl FnOnce(&Path)) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let workdir = dir.path().join("work");
    std::fs::create_dir_all(&workdir).expect("workdir");
    unsafe { std::env::set_var("HOME", dir.path()) };
    f(dir.path());
    unsafe { std::env::remove_var("HOME") };
}

fn make_config(dir: &Path) -> PathBuf {
    let db = dir.join("test.db");
    let config = format!(
        "version: 1\nmode: offline\ninference: {{ engines: [] }}\nagent: {{}}\nserver: {{ http: {{ enabled: false, bind: \"127.0.0.1:0\" }} }}\nsession: {{ db_path: \"{}\" }}\n",
        db.display()
    );
    let path = dir.join("config.toml");
    std::fs::write(&path, config).expect("write config");
    path
}

// ── scripted adapters ────────────────────────────────────────────────────

fn text_chunks(text: &str) -> Vec<Result<StreamChunk, LlmError>> {
    vec![
        Ok(StreamChunk::BlockStart {
            index: 0,
            block_type: ContentBlockType::Text,
        }),
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
    ]
}

fn tool_call_chunks(call: &str, args: &str) -> Vec<Result<StreamChunk, LlmError>> {
    vec![
        Ok(StreamChunk::ToolCallDelta {
            index: 0,
            id: starriver_kernel::llm::CallId(call.into()),
            name: Some("bash".into()),
            arguments_delta: args.to_string(),
        }),
        Ok(StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: starriver_kernel::llm::CallId(call.into()),
                name: Some("bash".into()),
                arguments: starriver_kernel::json::JsonValue::from_json_str(args)
                    .unwrap_or_else(|_| starriver_kernel::json::JsonValue::String(args.into())),
            },
        }),
        Ok(StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
            replay_state: None,
        }),
    ]
}

/// One text turn per call; records every prompt's text.
struct TextAdapter {
    prompts: Mutex<Vec<String>>,
    /// True once any message carried an Image content block.
    saw_image: std::sync::atomic::AtomicBool,
}

impl LlmAdapter for TextAdapter {
    fn providers(&self) -> &[String] {
        static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        PROVIDERS.get_or_init(|| vec!["mock".into()])
    }
    fn stream(&self, options: GenerateOptions) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
        for message in &options.messages {
            for block in &message.content {
                if matches!(block, ContentBlock::Image { .. }) {
                    self.saw_image
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        let text = options
            .messages
            .iter()
            .flat_map(|message| {
                message.content.iter().filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.clone()),
                    _ => None,
                })
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.prompts.lock().unwrap().push(text);
        Box::pin(futures::stream::iter(text_chunks("all done")))
    }
    fn chat(
        &self,
        _options: GenerateOptions,
    ) -> futures::future::BoxFuture<'static, Result<String, LlmError>> {
        Box::pin(async { Ok("all done".into()) })
    }
}

/// Tool round first (bash echo), then a text turn — exercises the approval
/// bridge on the first call.
struct ToolThenTextAdapter;

impl LlmAdapter for ToolThenTextAdapter {
    fn providers(&self) -> &[String] {
        static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
        PROVIDERS.get_or_init(|| vec!["mock".into()])
    }
    fn stream(&self, options: GenerateOptions) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
        let saw_tool_result = options.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::ToolResult { is_error: false, .. }
                )
            })
        });
        if saw_tool_result {
            Box::pin(futures::stream::iter(text_chunks("tool round complete")))
        } else {
            Box::pin(futures::stream::iter(tool_call_chunks(
                "call-1",
                r#"{"command":"echo acp-permission"}"#,
            )))
        }
    }
    fn chat(
        &self,
        _options: GenerateOptions,
    ) -> futures::future::BoxFuture<'static, Result<String, LlmError>> {
        Box::pin(async { Ok("ok".into()) })
    }
}

// ── channel transport + scripted client ─────────────────────────────────

struct ChannelRead {
    incoming: Mutex<std::sync::mpsc::Receiver<String>>,
}

impl AcpRead for ChannelRead {
    fn read_line(&mut self) -> Option<String> {
        self.incoming.lock().unwrap().recv().ok()
    }
}

struct ChannelWrite {
    outgoing: std::sync::mpsc::Sender<String>,
}

impl AcpWrite for ChannelWrite {
    fn write_line(&self, line: &str) {
        let _ = self.outgoing.send(line.to_string());
    }
}

struct Client {
    to_server: std::sync::mpsc::Sender<String>,
    from_server: Mutex<std::sync::mpsc::Receiver<String>>,
    updates: Mutex<Vec<Value>>,
}

impl Client {
    fn send(&self, message: Value) {
        self.to_server.send(message.to_string()).expect("send");
    }

    fn request(&self, id: u64, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Next inbound line, panicking instead of hanging on silence.
    fn next_line(&self) -> Value {
        let line = self
            .from_server
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(20))
            .expect("server line within 20s");
        serde_json::from_str(&line).expect("valid json")
    }

    /// Drain until the response to `id` arrives; every notification seen on
    /// the way lands in `updates` (and permission requests are returned).
    fn wait_response(&self, id: u64) -> Value {
        loop {
            let message = self.next_line();
            if message.get("id").and_then(Value::as_u64) == Some(id)
                && (message.get("result").is_some() || message.get("error").is_some())
            {
                return message;
            }
            if message.get("method").and_then(Value::as_str) == Some("session/update") {
                self.updates.lock().unwrap().push(message.clone());
            }
            if message.get("method").and_then(Value::as_str) == Some("session/request_permission")
            {
                self.updates.lock().unwrap().push(message.clone());
            }
        }
    }

    fn updates_of_type(&self, kind: &str) -> Vec<Value> {
        self.updates
            .lock()
            .unwrap()
            .iter()
            .filter(|update| {
                update
                    .pointer("/params/update/type")
                    .and_then(Value::as_str)
                    == Some(kind)
                    || update.get("method").and_then(Value::as_str)
                        == Some("session/request_permission")
            })
            .cloned()
            .collect()
    }
}

fn boot<A: LlmAdapter + 'static>(dir: &Path, adapter: Arc<A>) -> (Arc<AcpServer>, Client) {
    let host = HostRuntime::new(Some(make_config(dir).to_string_lossy().into_owned()))
        .expect("host boots");
    host.debug_register_adapter(adapter);
    let host = Arc::new(host);
    let (client_tx, server_rx) = std::sync::mpsc::channel::<String>();
    let (server_tx, client_rx) = std::sync::mpsc::channel::<String>();
    let server = Arc::new(AcpServer::new(
        host,
        Box::new(ChannelRead {
            incoming: Mutex::new(server_rx),
        }),
        Arc::new(ChannelWrite { outgoing: server_tx }),
    ));
    server.clone().spawn();
    let client = Client {
        to_server: client_tx,
        from_server: Mutex::new(client_rx),
        updates: Mutex::new(Vec::new()),
    };
    (server, client)
}

fn initialize(client: &Client) -> Value {
    client.request(1, "initialize", json!({"protocolVersion": 1, "clientCapabilities": {}}));
    let response = client.wait_response(1);
    assert!(response.get("result").is_some(), "initialize failed: {response}");
    client.notify("initialized", json!({}));
    response
}

fn new_session(client: &Client, id: u64, cwd: Option<&str>) -> String {
    client.request(
        id,
        "session/new",
        json!({"cwd": cwd.unwrap_or_default()}),
    );
    let response = client.wait_response(id);
    response["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string()
}

// ── tests ────────────────────────────────────────────────────────────────

#[test]
fn handshake_requires_initialize_before_sessions() {
    with_temp_home(|dir| {
        let (server, client) = boot(dir, Arc::new(TextAdapter { prompts: Mutex::new(vec![]), saw_image: std::sync::atomic::AtomicBool::new(false) }));
        let _ = server;

        client.request(1, "session/new", json!({}));
        let response = client.wait_response(1);
        assert!(response.get("error").is_some(), "pre-initialize must fail");

        let init = initialize(&client);
        assert_eq!(
            init["result"]["protocolVersion"].as_u64(),
            Some(1),
            "{init}"
        );
        assert_eq!(
            init["result"]["agentCapabilities"]["loadSession"].as_bool(),
            Some(true)
        );

        let sid = new_session(&client, 2, None);
        assert!(!sid.is_empty());
    });
}

#[test]
fn prompt_streams_updates_and_ends_the_turn() {
    with_temp_home(|dir| {
        let (server, client) = boot(
            dir,
            Arc::new(TextAdapter {
                prompts: Mutex::new(Vec::new()),
                saw_image: std::sync::atomic::AtomicBool::new(false),
            }),
        );
        let _ = server;
        initialize(&client);
        let sid = new_session(
            &client,
            2,
            Some(tempfile::tempdir().unwrap().path().to_str().unwrap()),
        );

        client.request(3, "session/prompt", json!({
            "sessionId": sid,
            "prompt": {"content": [{"type": "text", "text": "map the crate"}]}
        }));
        let response = client.wait_response(3);
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");

        let chunks = client.updates_of_type("agent_message_chunk");
        assert!(!chunks.is_empty(), "at least one delta chunk forwarded");
        let text: String = chunks
            .iter()
            .filter_map(|c| c.pointer("/params/update/content/0/text").and_then(Value::as_str))
            .collect();
        assert!(text.contains("all done"), "chunk text forwarded: {text}");
    });
}

#[test]
fn image_prompt_admits_attachment_into_task() {
    with_temp_home(|dir| {
        let adapter = Arc::new(TextAdapter {
            prompts: Mutex::new(vec![]),
            saw_image: std::sync::atomic::AtomicBool::new(false),
        });
        let (server, client) = boot(dir, Arc::clone(&adapter));
        let recorded = Arc::new(Mutex::new(Vec::<String>::new()));
        let _ = recorded;
        let _ = server;
        initialize(&client);
        let sid = new_session(&client, 2, None);

        // A real 1x1 PNG.
        let mut png = Vec::new();
        let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([9, 8, 7, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let b64 = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(&png)
        };

        client.request(3, "session/prompt", json!({
            "sessionId": sid,
            "prompt": {"content": [
                {"type": "text", "text": "what do you see"},
                {"type": "image", "data": b64, "mimeType": "image/png"}
            ]}
        }));
        let response = client.wait_response(3);
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");

        let task = adapter
            .prompts
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join("\n---\n");
        assert!(task.contains("what do you see"), "{task}");
        assert!(
            adapter.saw_image.load(std::sync::atomic::Ordering::SeqCst),
            "the admitted image rides the model-visible context as an Image block"
        );
        // (The `sha256:` ref itself is lifted out of the text into an Image
        // block by the host's extract_image_blocks — saw_image above is the
        // authoritative assertion.)
    });
}

#[test]
fn cancel_returns_cancelled_stop_reason() {
    with_temp_home(|dir| {
        // Cancel during a pending permission ask — the approval wait
        // selects on the cancel signal, so this is the deterministic
        // mid-turn cancellation point. (Cancelling while an LLM stream is
        // silent between chunks is not observable until the next chunk —
        // a pre-existing host behavior, out of ACP scope.)
        let (server, client) = boot(dir, Arc::new(ToolThenTextAdapter));
        let _ = server;
        initialize(&client);
        let sid = new_session(&client, 2, None);

        client.request(3, "session/prompt", json!({
            "sessionId": sid,
            "prompt": {"content": [{"type": "text", "text": "run the echo"}]}
        }));

        // Wait for the permission ask, then cancel instead of answering.
        loop {
            let message = client.next_line();
            if message.get("method").and_then(Value::as_str)
                == Some("session/request_permission")
            {
                client.updates.lock().unwrap().push(message.clone());
                break;
            }
            if message.get("id").and_then(Value::as_u64) == Some(3) {
                panic!("prompt resolved before the permission ask: {message}");
            }
        }
        client.notify("session/cancel", json!({"sessionId": sid}));
        let response = client.wait_response(3);
        assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    });
}

#[test]
fn permission_bridge_round_trips_allow_always() {
    with_temp_home(|dir| {
        let (server, client) = boot(dir, Arc::new(ToolThenTextAdapter));
        let _ = server;
        initialize(&client);
        let sid = new_session(&client, 2, None);

        client.request(3, "session/prompt", json!({
            "sessionId": sid,
            "prompt": {"content": [{"type": "text", "text": "run the echo"}]}
        }));

        // Wait for the permission request from the server (an outbound
        // JSON-RPC request, not a response), answer allow_always, then
        // collect the prompt response.
        let permission_id = loop {
            let message = client.next_line();
            if message.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                client.updates.lock().unwrap().push(message.clone());
                break message["id"].as_u64().expect("permission request id");
            }
            if message.get("id").and_then(Value::as_u64) == Some(3) {
                panic!("prompt resolved before the permission ask: {message}");
            }
        };
        client.send(json!({
            "jsonrpc": "2.0", "id": permission_id,
            "result": {"outcome": {"id": "allow_always"}}
        }));

        let response = client.wait_response(3);
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");

        let tool_calls = client.updates_of_type("tool_call");
        assert!(!tool_calls.is_empty(), "tool_call update forwarded");
        let tool_updates = client.updates_of_type("tool_call_update");
        assert!(!tool_updates.is_empty(), "tool_call_update forwarded");
    });
}

#[test]
fn load_session_validates_existence() {
    with_temp_home(|dir| {
        let (server, client) = boot(
            dir,
            Arc::new(TextAdapter {
                prompts: Mutex::new(vec![]),
                saw_image: std::sync::atomic::AtomicBool::new(false),
            }),
        );
        let _ = server;
        initialize(&client);

        client.request(2, "session/load", json!({"sessionId": "does-not-exist"}));
        let response = client.wait_response(2);
        assert!(response.get("error").is_some(), "unknown session errors");

        let sid = new_session(&client, 3, None);
        client.request(4, "session/prompt", json!({
            "sessionId": sid,
            "prompt": {"content": [{"type": "text", "text": "hi"}]}
        }));
        assert_eq!(client.wait_response(4)["result"]["stopReason"], "end_turn");

        client.request(5, "session/load", json!({"sessionId": sid}));
        let response = client.wait_response(5);
        assert_eq!(response["result"]["sessionId"].as_str(), Some(sid.as_str()));
    });
}

// Silence the unused ampsc import if the gated adapter drops it.
#[allow(dead_code)]
type _Unused = ampsc::Sender<u8>;

/// WI-108: cancelling while the provider stream is silent (hung provider)
/// now resolves the prompt immediately — previously the ACP layer had to
/// anchor cancel tests at the approval wait because this exact scenario
/// never delivered a terminal event.
#[test]
fn cancel_during_silent_llm_stream_returns_cancelled() {
    with_temp_home(|dir| {
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// One chunk, then the stream sits silent forever.
        struct SilentAdapter {
            entered: std::sync::atomic::AtomicUsize,
        }
        impl LlmAdapter for SilentAdapter {
            fn providers(&self) -> &[String] {
                static PROVIDERS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
                PROVIDERS.get_or_init(|| vec!["mock".into()])
            }
            fn stream(
                &self,
                _options: GenerateOptions,
            ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
                self.entered.fetch_add(1, Ordering::SeqCst);
                let first = Ok::<_, LlmError>(StreamChunk::BlockStart {
                    index: 0,
                    block_type: ContentBlockType::Text,
                });
                let silent =
                    futures::stream::once(futures::future::pending::<Result<StreamChunk, LlmError>>());
                Box::pin(futures::stream::once(async { first }).chain(silent))
            }
            fn chat(
                &self,
                _options: GenerateOptions,
            ) -> futures::future::BoxFuture<'static, Result<String, LlmError>> {
                Box::pin(async { Ok("ok".into()) })
            }
        }

        let adapter = Arc::new(SilentAdapter {
            entered: std::sync::atomic::AtomicUsize::new(0),
        });
        let (server, client) = boot(dir, adapter.clone());
        let _ = server;
        initialize(&client);
        let sid = new_session(&client, 2, None);

        client.request(3, "session/prompt", json!({
            "sessionId": sid,
            "prompt": {"content": [{"type": "text", "text": "park"}]}
        }));
        // Wait for the turn to enter the (silent) stream, then cancel —
        // no further chunk will ever arrive from the adapter.
        for _ in 0..500 {
            if adapter.entered.load(Ordering::SeqCst) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            adapter.entered.load(Ordering::SeqCst) > 0,
            "turn never started streaming"
        );
        client.notify("session/cancel", json!({"sessionId": sid}));
        let response = client.wait_response(3);
        assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    });
}

/// WI-111: outputStyle is rejected — we advertise outputStyle=false, and a
/// client that passes one anyway gets an explicit contract error.
#[test]
fn prompt_with_output_style_is_rejected() {
    with_temp_home(|dir| {
        let (server, client) = boot(
            dir,
            Arc::new(TextAdapter {
                prompts: Mutex::new(vec![]),
                saw_image: std::sync::atomic::AtomicBool::new(false),
            }),
        );
        let _ = server;
        initialize(&client);
        let sid = new_session(&client, 2, None);
        client.request(3, "session/prompt", json!({
            "sessionId": sid,
            "prompt": {
                "content": [{"type": "text", "text": "hi"}],
                "outputStyle": {"id": "concise", "name": "Concise"},
            }
        }));
        let response = client.wait_response(3);
        assert!(response.get("error").is_some(), "{response}");
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("outputStyle"),
            "{response}"
        );
    });
}
