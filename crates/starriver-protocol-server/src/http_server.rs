//! P4a/P4b protocol server: loopback HTTP + SSE, going through the same
//! `MethodRegistry` dispatch as IPC (one contract, different transports).
//! Shared by the embedded desktop server and the daemon (`starriverd`).
//!
//! Security model: bind `127.0.0.1` only; generate a random token per
//! startup (`invoke` uses `Authorization: Bearer`; SSE uses `?token=`
//! due to EventSource limitations); `/health` serves readiness probes
//! (a future daemon reuses the same implementation).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use starriver_protocol::wire::{ApiError, EventEnvelope, Request, Response};

use serde_json::Value;

/// Protocol dispatcher: shared by HTTP and a future daemon.
pub type Dispatcher = Arc<
    dyn Fn(&Request, Option<Arc<dyn Fn(EventEnvelope) + Send + Sync>>) -> Result<Response, ApiError>
        + Send
        + Sync,
>;

/// Handle to the running embedded server.
pub struct HttpServer {
    port: u16,
    token: String,
    /// Process instance id (sent with serverHello; regenerated on every spawn).
    instance_id: String,
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
    /// Shared event sink (seq-stamp + replay history + broadcast). Hoisted
    /// out of `serve()` so the embedded runtime can tee events that did NOT
    /// arrive over HTTP — NAPI in-process runs forward only to the Electron
    /// renderer — into the same SSE/WS stream remote clients subscribe to.
    /// Without this tee, an embedded-mode desktop's own activity is invisible
    /// to the paired phone (real-time sync starved).
    emit: Arc<dyn Fn(EventEnvelope) + Send + Sync>,
}

/// Event history capacity: replay window for reconnects (anything older is
/// backstopped by the `session.messages` snapshot).
const EVENT_HISTORY_CAP: usize = 8192;

/// WI-113: the minimal remote UI shell (event feed + approval decisions
/// over the WS mux). Single self-contained file; the mux connection still
/// requires the daemon token.
const REMOTE_UI_HTML: &[u8] = include_bytes!("../../../remote_ui.html");

impl HttpServer {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Emit an externally-produced event into the shared SSE/WS bus: stamped
    /// with the next global_seq, recorded to the replay history, broadcast to
    /// live subscribers. Embedded-mode bridge entry point.
    pub fn emit(&self, envelope: EventEnvelope) {
        (self.emit)(envelope)
    }

    /// Clonable emit sink for async contexts that must outlive a borrow of
    /// `HttpServer` (the embedded runtime's per-run relay tasks).
    pub fn emit_handle(&self) -> Arc<dyn Fn(EventEnvelope) + Send + Sync> {
        Arc::clone(&self.emit)
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
        // Backstop: abort the accept-loop task in addition to the cancel
        // token (idempotent).
        self.handle.abort();
    }
}

/// Spawn knobs: bind address and a fixed token. Defaults preserve the
/// historical loopback-random-token behavior.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// `ip:port` for `TcpListener::bind`. Loopback + random port by default;
    /// a LAN/VPN-facing bind (e.g. `0.0.0.0:7878`) is how remote clients
    /// (iPhone companion) reach the daemon.
    pub bind: Option<String>,
    /// Fixed bearer token. When absent a random one is generated per start;
    /// daemons that must survive restarts without re-pairing clients pass a
    /// persistent value.
    pub token: Option<String>,
    /// SSE keepalive comment interval in seconds. The phone's stream watchdog
    /// treats a silent connection as dead (a half-open socket otherwise hangs
    /// `bytes.lines` forever with `.infinity` timeouts); healthy streams see
    /// `: keepalive` every `heartbeat_secs` and stay fed.
    pub heartbeat_secs: u64,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            bind: None,
            token: None,
            heartbeat_secs: 20,
        }
    }
}

/// Starts the protocol server on a random loopback port (legacy behavior).
pub async fn spawn(
    dispatch: Dispatcher,
    runtime: tokio::runtime::Handle,
) -> anyhow::Result<HttpServer> {
    spawn_with(dispatch, runtime, SpawnOptions::default()).await
}

/// Starts the protocol server with explicit bind/token.
pub async fn spawn_with(
    dispatch: Dispatcher,
    runtime: tokio::runtime::Handle,
    options: SpawnOptions,
) -> anyhow::Result<HttpServer> {
    let bind = options.bind.unwrap_or_else(|| "127.0.0.1:0".to_string());
    let listener = TcpListener::bind(&bind).await?;
    let port = listener.local_addr()?.port();
    let token = options
        .token
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let cancel = CancellationToken::new();
    // Shared event bus (SSE/WS subscribers + external emitters): hoisted out
    // of serve() so the embedded runtime can tee its in-process events into
    // the same stream remote clients subscribe to.
    let events = tokio::sync::broadcast::channel::<EventEnvelope>(512).0;
    let history: Arc<Mutex<VecDeque<(u64, EventEnvelope)>>> = Arc::new(Mutex::new(VecDeque::new()));
    let next_seq = Arc::new(AtomicU64::new(0));
    let heartbeat = std::time::Duration::from_secs(options.heartbeat_secs.max(1));
    // Process instance id: sent with the event stream so clients can detect a server restart
    // (global_seq restarts from 0, so a client's persisted stale after= watermark would filter out all new events).
    let instance_id = uuid::Uuid::new_v4().simple().to_string();
    let emit = make_emit(&events, &history, &next_seq).expect("emit sink always constructible");
    let handle = runtime.spawn(serve(
        listener,
        dispatch,
        token.clone(),
        cancel.clone(),
        events,
        history,
        next_seq,
        heartbeat,
        instance_id.clone(),
    ));
    Ok(HttpServer {
        port,
        token,
        instance_id,
        cancel,
        handle,
        emit,
    })
}

async fn serve(
    listener: TcpListener,
    dispatch: Dispatcher,
    token: String,
    cancel: CancellationToken,
    events: tokio::sync::broadcast::Sender<EventEnvelope>,
    history: Arc<Mutex<VecDeque<(u64, EventEnvelope)>>>,
    next_seq: Arc<AtomicU64>,
    heartbeat: std::time::Duration,
    instance_id: String,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let dispatch = Arc::clone(&dispatch);
                let token = token.clone();
                let events = events.clone();
                let history = Arc::clone(&history);
                let next_seq = Arc::clone(&next_seq);
                let instance_id = instance_id.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(
                        stream, dispatch, token, events, history, next_seq, heartbeat,
                        instance_id,
                    )
                    .await;
                });
            }
        }
    }
}

struct HttpRequest {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// Bytes already read past `Content-Length` (a WebSocket client may
    /// pipeline its first frame with the handshake GET — the upgrade path
    /// MUST replay them into the frame reader or the first frame is lost).
    #[allow(dead_code)]
    leftover: Vec<u8>,
}

async fn handle_connection(
    mut stream: TcpStream,
    dispatch: Dispatcher,
    token: String,
    events: tokio::sync::broadcast::Sender<EventEnvelope>,
    history: Arc<Mutex<VecDeque<(u64, EventEnvelope)>>>,
    next_seq: Arc<AtomicU64>,
    heartbeat: std::time::Duration,
    instance_id: String,
) -> anyhow::Result<()> {
    let request = match read_request(&mut stream).await {
        Ok(request) => request,
        Err(error) => {
            write_response(
                &mut stream,
                400,
                "application/json",
                error.to_string().as_bytes(),
            )
            .await?;
            return Ok(());
        }
    };

    if request.method == "GET" && request.path == "/health" {
        return write_response(&mut stream, 200, "text/plain", b"ok").await;
    }

    // WI-113: the minimal remote UI — a self-contained page that speaks the
    // WS mux (event feed + approval decisions). Serving the shell needs no
    // auth; the mux connection itself still requires the token.
    if request.method == "GET" && (request.path == "/remote" || request.path == "/remote/") {
        return write_response(&mut stream, 200, "text/html", REMOTE_UI_HTML).await;
    }

    if request.method == "POST" && request.path == "/api/v1/invoke" {
        if !authorized(&request, &token) {
            return write_response(
                &mut stream,
                401,
                "application/json",
                br#"{"error":{"code":"UNAUTHORIZED","message":"missing or invalid token"}}"#,
            )
            .await;
        }
        let request: Request = match serde_json::from_slice(&request.body) {
            Ok(request) => request,
            Err(_) => {
                return write_response(
                    &mut stream,
                    400,
                    "application/json",
                    br#"{"error":{"code":"INVALID_PARAMS","message":"bad request JSON"}}"#,
                )
                .await;
            }
        };
        let emit = make_emit(&events, &history, &next_seq);
        // Host methods make heavy use of `blocking_lock()`/`block_on`, so we
        // must not dispatch on a tokio worker thread (it would panic with
        // "Cannot block the current thread from within a runtime").
        // spawn_blocking runs it on a blocking-pool thread, keeping the
        // behavior identical for the shared IPC/HTTP dispatcher.
        let dispatch_task = {
            let dispatch = Arc::clone(&dispatch);
            let request = request;
            let emit = emit;
            tokio::task::spawn_blocking(move || {
                dispatch(&request, emit).unwrap_or_else(|error| Response::err(&request, error))
            })
        };
        let response = dispatch_task
            .await
            .map_err(|e| anyhow::anyhow!("dispatch task failed: {e}"))?;
        let body = serde_json::to_vec(&response)?;
        return write_response(&mut stream, 200, "application/json", &body).await;
    }

    if request.method == "GET" && request.path == "/api/v1/mux" {
        // WI-18 E2: single-connection WebSocket mux — invoke + events +
        // approval decisions on one bidirectional channel.
        let query_token = request
            .query
            .as_deref()
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")));
        let via_header = authorized(&request, &token);
        if query_token != Some(token.as_str()) && !via_header {
            return write_response(&mut stream, 401, "application/json",
                br#"{"error":{"code":"UNAUTHORIZED","message":"missing or invalid token"}}"#,
            ).await;
        }
        let key = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
            .map(|(_, value)| value.clone());
        let wants_upgrade = request
            .headers
            .iter()
            .any(|(name, value)| {
                name.eq_ignore_ascii_case("upgrade") && value.eq_ignore_ascii_case("websocket")
            });
        let (Some(key), true) = (key, wants_upgrade) else {
            return write_response(&mut stream, 400, "text/plain", b"websocket upgrade required").await;
        };
        let after = request
            .query
            .as_deref()
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("after=")))
            .and_then(|v| v.parse::<u64>().ok());
        let session = request
            .query
            .as_deref()
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("session=")))
            .map(str::to_string);
        return serve_mux(
            &mut stream,
            dispatch,
            events,
            history,
            next_seq,
            &key,
            after,
            session,
            request.leftover,
        )
        .await;
    }

    if request.method == "GET" && request.path == "/api/v1/events" {
        let query_token = request
            .query
            .as_deref()
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=")));
        if query_token != Some(token.as_str()) {
            return write_response(&mut stream, 401, "text/plain", b"unauthorized").await;
        }
        let after = request
            .query
            .as_deref()
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("after=")))
            .and_then(|v| v.parse::<u64>().ok());
        // WI-50 session-scoped subscription: only receive events of one
        // session (approval routing key). Backward compatible — clients that
        // omit it keep the full broadcast.
        let session = request
            .query
            .as_deref()
            .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("session=")))
            .map(str::to_string);
        return serve_sse(&mut stream, events, history, after, session, heartbeat, &instance_id, &next_seq).await;
    }

    write_response(&mut stream, 404, "text/plain", b"not found").await
}

/// The dispatcher's event sink: stamp a global sequence number, record to
/// the replay history, then broadcast to live subscribers. Shared by the
/// HTTP invoke path and the WS mux channel (identical semantics).
fn make_emit(
    events: &tokio::sync::broadcast::Sender<EventEnvelope>,
    history: &Arc<Mutex<VecDeque<(u64, EventEnvelope)>>>,
    next_seq: &Arc<AtomicU64>,
) -> Option<Arc<dyn Fn(EventEnvelope) + Send + Sync>> {
    let events = events.clone();
    let history = Arc::clone(history);
    let next_seq = Arc::clone(next_seq);
    Some(Arc::new(move |envelope: EventEnvelope| {
        let global_seq = next_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut envelope = envelope;
        envelope.global_seq = Some(global_seq);
        {
            let mut history = history.lock().unwrap_or_else(|e| e.into_inner());
            if history.len() >= EVENT_HISTORY_CAP {
                history.pop_front();
            }
            history.push_back((global_seq, envelope.clone()));
        }
        let _ = events.send(envelope);
    }))
}

fn authorized(request: &HttpRequest, token: &str) -> bool {
    request.headers.iter().any(|(key, value)| {
        key.eq_ignore_ascii_case("authorization")
            && value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
                .map(|v| v.trim())
                == Some(token)
    }) || request
        .headers
        .iter()
        .any(|(key, value)| key.eq_ignore_ascii_case("x-starriver-token") && value == token)
}

/// WI-50: an envelope belongs to `session` when either its subscription id
/// or the UiEvent payload's `sessionId` names it — the two spellings of the
/// same routing key across streaming replies and host UiEvents.
fn envelope_for_session(envelope: &EventEnvelope, session: &str) -> bool {
    envelope.subscription == session
        || envelope.payload.get("sessionId").and_then(Value::as_str) == Some(session)
}

/// A reader that yields the swallowed handshake-pipelined bytes first,
/// then the socket. `AsyncReadExt::read_exact` works through `AsyncRead`.
struct PrefixedReader<R> {
    prefix: std::io::Cursor<Vec<u8>>,
    inner: R,
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for PrefixedReader<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.prefix.position() < self.prefix.get_ref().len() as u64 {
            // Synchronous prefix drain: Cursor read is infallible here.
            let remaining = self.prefix.get_ref().len() as u64 - self.prefix.position();
            let chunk = remaining.min(buf.remaining() as u64) as usize;
            let start = self.prefix.position() as usize;
            let bytes = self.prefix.get_ref()[start..start + chunk].to_vec();
            self.prefix.set_position((start + chunk) as u64);
            buf.put_slice(&bytes);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

/// Writer-channel frame: data text frames and pongs serialize together.
enum MuxFrame {
    Text(String),
    Pong(Vec<u8>),
}

/// WI-18 E2/E3: the WebSocket mux session. After the 101 handshake the
/// single connection carries everything: client `Request` frames (including
/// `approval.respond` decisions — dispatched through the same catalog, so
/// WI-16 first-wins arbitration applies unchanged), server `Response`
/// frames (id-correlated), and live `EventEnvelope` pushes. Reconnect
/// replay uses the same `(after, watermark]` semantics as SSE.
#[allow(clippy::too_many_arguments)]
async fn serve_mux(
    stream: &mut TcpStream,
    dispatch: Dispatcher,
    events: tokio::sync::broadcast::Sender<EventEnvelope>,
    history: Arc<Mutex<VecDeque<(u64, EventEnvelope)>>>,
    next_seq: Arc<AtomicU64>,
    ws_key: &str,
    after: Option<u64>,
    session: Option<String>,
    leftover: Vec<u8>,
) -> anyhow::Result<()> {
    use crate::ws::{self, WsMessage};

    // Complete the upgrade handshake, then split the socket: the read loop
    // owns the read half, the writer task owns the write half.
    let accept = ws::accept_key(ws_key);
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    // A pipelined first frame rides here; prepend it ahead of the socket.
    let mut read_half = PrefixedReader {
        prefix: std::io::Cursor::new(leftover),
        inner: read_half,
    };

    // Subscribe BEFORE snapshotting history (the SSE ordering rule:
    // replay and live never gap and never duplicate).
    let mut receiver = events.subscribe();
    if let Some(after) = after {
        let replay: Vec<EventEnvelope> = {
            let history = history.lock().unwrap_or_else(|e| e.into_inner());
            history
                .iter()
                .filter(|(seq, _)| *seq > after)
                .map(|(_, envelope)| envelope.clone())
                .filter(|envelope| {
                    session
                        .as_deref()
                        .is_none_or(|session| envelope_for_session(envelope, session))
                })
                .collect()
        };
        for envelope in replay {
            ws::write_text(&mut write_half, &serde_json::to_string(&envelope)?).await?;
        }
    }

    // Everything outbound serializes through one channel: responses (from
    // blocking-pool dispatch), live event envelopes, and pongs.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<MuxFrame>(256);

    // Read loop: frames in, requests dispatched off the async workers.
    let read_dispatch = {
        let dispatch = Arc::clone(&dispatch);
        let tx = tx.clone();
        let emit = make_emit(&events, &history, &next_seq);
        async move {
            while let Ok(message) = ws::read_message(&mut read_half).await {
                match message {
                    WsMessage::Close => break,
                    WsMessage::Ping(payload) => {
                        if tx.send(MuxFrame::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    WsMessage::Text(text) => {
                        match serde_json::from_str::<Request>(&text) {
                            Ok(request) => {
                                let dispatch = Arc::clone(&dispatch);
                                let emit = emit.clone();
                                let tx = tx.clone();
                                // Host methods block internally — the same
                                // spawn_blocking rule as the HTTP invoke.
                                let task = tokio::task::spawn_blocking(move || {
                                    dispatch(&request, emit).unwrap_or_else(|error| {
                                        Response::err(&request, error)
                                    })
                                });
                                match task.await {
                                    Ok(response) => {
                                        let Ok(json) = serde_json::to_string(&response) else {
                                            continue;
                                        };
                                        if tx.send(MuxFrame::Text(json)).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                            Err(_) => {
                                // A malformed frame gets an error response
                                // keyed to the raw text so clients stay
                                // debuggable; the channel survives.
                                let response = Response {
                                    v: 1,
                                    id: text.chars().take(64).collect(),
                                    ok: false,
                                    result: None,
                                    error: Some(ApiError::new(
                                        "INVALID_PARAMS",
                                        "bad request JSON",
                                    )),
                                };
                                let Ok(json) = serde_json::to_string(&response) else {
                                    continue;
                                };
                                if tx.send(MuxFrame::Text(json)).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    // Event loop: live envelopes through the reconnect + session filters.
    let event_forward = async move {
        loop {
            match receiver.recv().await {
                Ok(envelope) => {
                    if let (Some(after), Some(global_seq)) = (after, envelope.global_seq) {
                        if global_seq <= after {
                            continue;
                        }
                    }
                    if session
                        .as_deref()
                        .is_some_and(|session| !envelope_for_session(&envelope, session))
                    {
                        continue;
                    }
                    let Ok(json) = serde_json::to_string(&envelope) else {
                        continue;
                    };
                    if tx.send(MuxFrame::Text(json)).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    };

    // Drive the readers concurrently; the writer drains until they finish.
    let readers = async {
        tokio::select! {
            _ = read_dispatch => {}
            _ = event_forward => {}
        }
    };
    let writer = async {
        while let Some(frame) = rx.recv().await {
            let outcome = match frame {
                MuxFrame::Text(json) => ws::write_text(&mut write_half, &json).await,
                MuxFrame::Pong(payload) => ws::write_pong(&mut write_half, &payload).await,
            };
            if outcome.is_err() {
                break;
            }
        }
        let _ = ws::write_close(&mut write_half).await;
    };
    tokio::join!(readers, writer);
    Ok(())
}

async fn serve_sse(
    stream: &mut TcpStream,
    events: tokio::sync::broadcast::Sender<EventEnvelope>,
    history: Arc<Mutex<VecDeque<(u64, EventEnvelope)>>>,
    after: Option<u64>,
    session: Option<String>,
    heartbeat: std::time::Duration,
    instance_id: &str,
    next_seq: &AtomicU64,
) -> anyhow::Result<()> {
    // Subscribe first, then replay: subscribing before snapshotting the
    // watermark guarantees no gap between replay and live events (no drops).
    let mut receiver = events.subscribe();
    // A continuous stream without Content-Length must use chunked encoding,
    // otherwise clients wait for EOF before yielding any data.
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;
    // serverHello: subscription confirmation + instance id + current watermark. The client (phone)
    // compares the persisted instance id and resets after= on change — global_seq restarts from
    // 0 after a restart, so a stale watermark would filter out all new events (the root cause of a permanently deaf phone). Must be written after the headers.
    {
        let watermark = next_seq.load(std::sync::atomic::Ordering::SeqCst);
        let hello = serde_json::json!({
            "type": "serverHello",
            "payload": { "instanceId": instance_id, "watermark": watermark },
        });
        let frame = format!("data: {hello}\n\n");
        let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
        stream.write_all(chunk.as_bytes()).await?;
        stream.flush().await?;
    }
    // Reconnect replay: resend history events in `(after, watermark]`, then
    // follow with live events (seq > watermark).
    if let Some(after) = after {
        let replay: Vec<EventEnvelope> = {
            let history = history.lock().unwrap_or_else(|e| e.into_inner());
            history
                .iter()
                .filter(|(seq, _)| *seq > after)
                .map(|(_, envelope)| envelope.clone())
                .filter(|envelope| {
                    session
                        .as_deref()
                        .is_none_or(|session| envelope_for_session(envelope, session))
                })
                .collect()
        };
        for envelope in replay {
            let json = serde_json::to_string(&envelope)?;
            let frame = format!("data: {json}\n\n");
            let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
            stream.write_all(chunk.as_bytes()).await?;
        }
        stream.flush().await?;
    }
    // SSE keepalive: an idle subscriber used to hang forever on a half-open
    // socket (no bytes → the client's read never errors → no reconnect, the
    // phone froze with a green "connected" bar). A comment frame every
    // `heartbeat` keeps NATs/proxies fed, lets the client's idle watchdog
    // distinguish live from dead, and surfaces a dead peer as a write error
    // so this task tears the connection down instead of leaking it.
    let mut keepalive = tokio::time::interval(heartbeat);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await; // interval fires immediately once; consume it
    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                let frame = ": keepalive\n\n";
                let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
                if stream.write_all(chunk.as_bytes()).await.is_err() {
                    break; // peer gone — drop the subscriber task
                }
                let _ = stream.flush().await;
            }
            msg = receiver.recv() => {
                match msg {
                    Ok(envelope) => {
                        if let (Some(after), Some(global_seq)) = (after, envelope.global_seq) {
                            if global_seq <= after {
                                continue;
                            }
                        }
                        if session
                            .as_deref()
                            .is_some_and(|session| !envelope_for_session(&envelope, session))
                        {
                            continue;
                        }
                        let json = serde_json::to_string(&envelope)?;
                        let frame = format!("data: {json}\n\n");
                        let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
                        stream.write_all(chunk.as_bytes()).await?;
                        stream.flush().await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    Ok(())
}

async fn read_request(stream: &mut TcpStream) -> anyhow::Result<HttpRequest> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("connection closed before headers");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos;
            break;
        }
        if buf.len() > 64 * 1024 {
            anyhow::bail!("request headers too large");
        }
    }

    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let mut parts = lines.next().unwrap_or_default().split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let raw_path = parts.next().unwrap_or_default().to_string();
    let (path, query) = match raw_path.split_once('?') {
        Some((path, query)) => (path.to_string(), Some(query.to_string())),
        None => (raw_path, None),
    };
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            if key.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((key, value));
        }
    }
    while buf.len() < header_end + 4 + content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("connection closed before body");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[header_end + 4..header_end + 4 + content_length].to_vec();
    let leftover = buf[header_end + 4 + content_length..].to_vec();
    Ok(HttpRequest {
        method,
        path,
        query,
        headers,
        body,
        leftover,
    })
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Status",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_protocol::wire::Request as ProtocolRequest;

    fn fake_dispatcher() -> Dispatcher {
        Arc::new(
            |request: &ProtocolRequest, emit: Option<Arc<dyn Fn(EventEnvelope) + Send + Sync>>| {
                if request.method == "session.execute" {
                    if let Some(emit) = emit {
                        emit(EventEnvelope::new(
                            "sub_1",
                            1,
                            "turnStart",
                            serde_json::json!({}),
                        ));
                    }
                    return Ok(Response::ok(
                        request,
                        serde_json::json!({ "subscription": "sub_1" }),
                    ));
                }
                if request.method == "test.emitSessions" {
                    // WI-50 test aid: one event routed by subscription id and
                    // one routed by the UiEvent payload's sessionId.
                    if let Some(emit) = emit {
                        emit(EventEnvelope::new(
                            "sess-a",
                            1,
                            "approvalRequested",
                            serde_json::json!({ "sessionId": "sess-a" }),
                        ));
                        emit(EventEnvelope::new(
                            "sess-b",
                            1,
                            "approvalRequested",
                            serde_json::json!({ "sessionId": "sess-b" }),
                        ));
                    }
                    return Ok(Response::ok(request, serde_json::json!({})));
                }
                Ok(Response::ok(
                    request,
                    serde_json::json!({ "echo": request.method }),
                ))
            },
        )
    }

    async fn start() -> (HttpServer, String) {
        let server = spawn(fake_dispatcher(), tokio::runtime::Handle::current())
            .await
            .unwrap();
        let base = format!("http://127.0.0.1:{}", server.port());
        (server, base)
    }

    /// Server with a 1s heartbeat so the keepalive test stays fast.
    async fn start_fast_heartbeat() -> (HttpServer, String) {
        let server = spawn_with(
            fake_dispatcher(),
            tokio::runtime::Handle::current(),
            SpawnOptions {
                heartbeat_secs: 1,
                ..SpawnOptions::default()
            },
        )
        .await
        .unwrap();
        let base = format!("http://127.0.0.1:{}", server.port());
        (server, base)
    }

    async fn post(base: &str, path: &str, token: Option<&str>, body: &[u8]) -> (u16, Vec<u8>) {
        let client = reqwest::Client::new();
        let mut request = client.post(format!("{base}{path}")).body(body.to_vec());
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let bytes = response.bytes().await.unwrap().to_vec();
        (status, bytes)
    }

    #[tokio::test]
    async fn health_endpoint_responds() {
        let (server, base) = start().await;
        let client = reqwest::Client::new();
        let response = client.get(format!("{base}/health")).send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        server.shutdown();
    }

    #[tokio::test]
    async fn invoke_requires_token() {
        let (server, base) = start().await;
        let request = serde_json::to_vec(&ProtocolRequest {
            v: 1,
            id: "r1".into(),
            method: "tools.list".into(),
            params: serde_json::json!({}),
        })
        .unwrap();
        let (status, _) = post(&base, "/api/v1/invoke", None, &request).await;
        assert_eq!(status, 401);
        server.shutdown();
    }

    #[tokio::test]
    async fn invoke_dispatches_with_token() {
        let (server, base) = start().await;
        let request = serde_json::to_vec(&ProtocolRequest {
            v: 1,
            id: "r2".into(),
            method: "tools.list".into(),
            params: serde_json::json!({}),
        })
        .unwrap();
        let (status, body) = post(&base, "/api/v1/invoke", Some(server.token()), &request).await;
        assert_eq!(status, 200);
        let response: Response = serde_json::from_slice(&body).unwrap();
        assert!(response.ok);
        assert_eq!(response.result.unwrap()["echo"], "tools.list");
        server.shutdown();
    }

    #[tokio::test]
    async fn invoke_rejects_bad_json() {
        let (server, base) = start().await;
        let (status, _) = post(&base, "/api/v1/invoke", Some(server.token()), b"not json").await;
        assert_eq!(status, 400);
        server.shutdown();
    }

    #[tokio::test]
    async fn sse_streams_events_from_streaming_methods() {
        let (server, base) = start().await;
        let addr = format!("127.0.0.1:{}", server.port());
        let token = server.token().to_string();
        let (established, established_rx) = tokio::sync::oneshot::channel::<()>();
        let sse = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let mut stream = TcpStream::connect(&addr).await.unwrap();
                stream
                    .write_all(
                        format!(
                            "GET /api/v1/events?token={token} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                stream.flush().await.unwrap();
                let _ = established.send(());
                let mut all = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    all.extend_from_slice(&buf[..n]);
                    if all.windows(9).any(|w| w == b"turnStart") {
                        break;
                    }
                }
                String::from_utf8_lossy(&all).to_string()
            })
            .await
            .expect("sse timed out")
        });

        // Wait until the SSE is subscribed (200 head received) before
        // triggering events, so broadcasts don't drop frames with no
        // receivers.
        established_rx.await.unwrap();
        let request = serde_json::to_vec(&ProtocolRequest {
            v: 1,
            id: "r3".into(),
            method: "session.execute".into(),
            params: serde_json::json!({ "task": "x" }),
        })
        .unwrap();
        let (status, _) = post(&base, "/api/v1/invoke", Some(server.token()), &request).await;
        assert_eq!(status, 200);

        let sse_text = sse.await.unwrap();
        assert!(sse_text.contains("data: "), "{sse_text}");
        assert!(
            sse_text.contains("\"subscription\":\"sub_1\""),
            "{sse_text}"
        );
        server.shutdown();
    }

    /// SSE keepalive: an idle subscriber must still receive `: keepalive`
    /// comment frames — the phone's watchdog relies on them to tell a live
    /// stream from a half-open socket (which used to freeze the transcript
    /// forever with a green "connected" bar).
    #[tokio::test]
    async fn sse_idle_stream_receives_keepalive_comments() {
        let (server, _base) = start_fast_heartbeat().await;
        let addr = format!("127.0.0.1:{}", server.port());
        let mut stream = open_sse(&addr, server.token(), None).await;
        // Read whatever arrives within ~3s (no events are broadcast — only
        // heartbeats should flow) and count the comment frames.
        let text = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut all = Vec::new();
            let mut buf = [0u8; 1024];
            while String::from_utf8_lossy(&all).matches(": keepalive").count() < 2 {
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                all.extend_from_slice(&buf[..n]);
            }
            String::from_utf8_lossy(&all).to_string()
        })
        .await
        .unwrap_or_default();
        let hits = text.matches(": keepalive").count();
        assert!(hits >= 2, "expected ≥2 keepalive frames, got {hits}: {text}");
        server.shutdown();
    }

    async fn open_sse(addr: &str, token: &str, after: Option<u64>) -> TcpStream {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let path = match after {
            Some(seq) => format!("/api/v1/events?token={token}&after={seq}"),
            None => format!("/api/v1/events?token={token}"),
        };
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        stream.flush().await.unwrap();
        stream
    }

    async fn read_sse_until(stream: &mut TcpStream, needle: &str) -> String {
        let mut all = Vec::new();
        let mut buf = [0u8; 4096];
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                all.extend_from_slice(&buf[..n]);
                if all.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                    break;
                }
            }
        })
        .await
        .expect("sse timed out");
        String::from_utf8_lossy(&all).to_string()
    }

    fn execute_body() -> Vec<u8> {
        serde_json::to_vec(&ProtocolRequest {
            v: 1,
            id: "r-exec".into(),
            method: "session.execute".into(),
            params: serde_json::json!({ "task": "x" }),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn sse_replays_history_after_disconnect_with_after_param() {
        let (server, base) = start().await;
        let addr = format!("127.0.0.1:{}", server.port());
        let token = server.token().to_string();

        // 1) First subscriber receives an event (global_seq=1), then
        //    disconnects.
        let mut first = open_sse(&addr, &token, None).await;
        let (status, _) = post(&base, "/api/v1/invoke", Some(&token), &execute_body()).await;
        assert_eq!(status, 200);
        let first_text = read_sse_until(&mut first, "\"global_seq\":1").await;
        assert!(first_text.contains("\"global_seq\":1"), "{first_text}");
        drop(first);

        // 2) Trigger another event while disconnected (no receivers, but it
        //    enters the history buffer).
        let (status, _) = post(&base, "/api/v1/invoke", Some(&token), &execute_body()).await;
        assert_eq!(status, 200);

        // 3) Reconnect with after=1: replay only global_seq=2, not the
        //    already-received 1.
        let mut replay = open_sse(&addr, &token, Some(1)).await;
        let replay_text = read_sse_until(&mut replay, "\"global_seq\":2").await;
        assert!(replay_text.contains("\"global_seq\":2"), "{replay_text}");
        assert!(
            !replay_text.contains("\"global_seq\":1"),
            "after=1 must not replay already-seen events: {replay_text}"
        );
        server.shutdown();
    }

    #[test]
    fn session_routing_predicate_matches_both_key_spellings() {
        let sub = EventEnvelope::new("sess-a", 1, "turnStart", serde_json::json!({}));
        assert!(envelope_for_session(&sub, "sess-a"));
        assert!(!envelope_for_session(&sub, "sess-b"));
        let payload = EventEnvelope::new(
            "sub_9",
            1,
            "approvalRequested",
            serde_json::json!({ "sessionId": "sess-b" }),
        );
        assert!(envelope_for_session(&payload, "sess-b"));
        assert!(!envelope_for_session(&payload, "sess-a"));
    }

    /// WI-50: a session-scoped SSE subscription only receives that session's
    /// events (approval requests included) — the minimal multi-client
    /// routing surface until a real user-identity model exists.
    #[tokio::test]
    async fn sse_session_filter_delivers_only_that_sessions_events() {
        let (server, base) = start().await;
        let addr = format!("127.0.0.1:{}", server.port());
        let token = server.token().to_string();

        // Scoped subscriber for sess-a.
        let mut scoped = TcpStream::connect(&addr).await.unwrap();
        scoped
            .write_all(
                format!(
                    "GET /api/v1/events?token={token}&session=sess-a HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        scoped.flush().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Emit one event for sess-a and one for sess-b.
        let request = serde_json::to_vec(&ProtocolRequest {
            v: 1,
            id: "r-emit".into(),
            method: "test.emitSessions".into(),
            params: serde_json::json!({}),
        })
        .unwrap();
        let (status, _) = post(&base, "/api/v1/invoke", Some(&token), &request).await;
        assert_eq!(status, 200);

        // Receive sess-a's event, then let a (filtered) sess-b frame settle
        // in before asserting it never arrives.
        let mut text = read_sse_until(&mut scoped, "\"global_seq\":1").await;
        assert!(text.contains("sess-a"), "{text}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let mut buf = [0u8; 4096];
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(150), scoped.read(&mut buf))
                .await
            {
                Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
                Ok(Ok(n)) => text.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
        assert!(
            !text.contains("sess-b"),
            "scoped subscription must not receive other sessions' events: {text}"
        );
        server.shutdown();
    }

    /// WI-50: an unscoped subscription keeps the full broadcast
    /// (backward compatibility).
    #[tokio::test]
    async fn sse_without_session_param_keeps_full_broadcast() {
        let (server, base) = start().await;
        let addr = format!("127.0.0.1:{}", server.port());
        let token = server.token().to_string();
        let mut all = open_sse(&addr, &token, None).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let request = serde_json::to_vec(&ProtocolRequest {
            v: 1,
            id: "r-emit".into(),
            method: "test.emitSessions".into(),
            params: serde_json::json!({}),
        })
        .unwrap();
        let (status, _) = post(&base, "/api/v1/invoke", Some(&token), &request).await;
        assert_eq!(status, 200);
        let text = read_sse_until(&mut all, "\"global_seq\":2").await;
        assert!(text.contains("sess-a") && text.contains("sess-b"), "{text}");
        server.shutdown();
    }
    // ── WI-18 E2/E3: WebSocket mux ────────────────────────────────────────

    /// Minimal WS test client: masked frames out, frames in.
    struct WsClient {
        stream: tokio::net::TcpStream,
    }

    impl WsClient {
        async fn connect(port: u16, token: &str, after: Option<u64>, session: Option<&str>) -> Self {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect");
            let mut query = format!("token={token}");
            if let Some(after) = after {
                query.push_str(&format!("&after={after}"));
            }
            if let Some(session) = session {
                query.push_str(&format!("&session={session}"));
            }
            let key = "dGhlIHNhbXBsZSBub25jZQ"; // RFC 6455 example key
            let request = format!(
                "GET /api/v1/mux?{query} HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).await.unwrap();
                head.extend_from_slice(&byte);
            }
            let head = String::from_utf8(head).unwrap();
            assert!(head.starts_with("HTTP/1.1 101"), "{head}");
            let expected = crate::ws::accept_key(key);
            assert!(
                head.contains(&format!("Sec-WebSocket-Accept: {expected}")),
                "accept header: {head}"
            );
            Self { stream }
        }

        async fn send_text(&mut self, text: &str) {
            use tokio::io::AsyncWriteExt;
            let payload = text.as_bytes();
            let mut frame = vec![0x81u8]; // FIN + text
            let mask_key = [0x2A, 0x17, 0x09, 0x5C];
            let len = payload.len();
            if len < 126 {
                frame.push(0x80 | len as u8);
            } else {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            }
            frame.extend_from_slice(&mask_key);
            for (index, byte) in payload.iter().enumerate() {
                frame.push(byte ^ mask_key[index % 4]);
            }
            self.stream.write_all(&frame).await.unwrap();
            self.stream.flush().await.unwrap();
        }

        /// Next text frame (skips pongs).
        async fn recv_text(&mut self) -> String {
            loop {
                let frame = self.recv_frame().await;
                let opcode = frame[0] & 0x0F;
                if opcode == 0xA {
                    continue; // pong
                }
                assert_eq!(opcode, 1, "expected text frame");
                return String::from_utf8(frame[header_len(&frame)..].to_vec()).unwrap();
            }
        }

        async fn recv_raw(&mut self) -> Vec<u8> {
            self.recv_frame().await
        }

        async fn recv_frame(&mut self) -> Vec<u8> {
            use tokio::io::AsyncReadExt;
            let stream = &mut self.stream;
            let mut head = [0u8; 2];
            stream.read_exact(&mut head).await.unwrap();
            let len7 = head[1] & 0x7F;
            let mut header_extra = 0usize;
            let len = match len7 {
                126 => {
                    let mut ext = [0u8; 2];
                    stream.read_exact(&mut ext).await.unwrap();
                    header_extra = 2;
                    u16::from_be_bytes(ext) as usize
                }
                127 => {
                    let mut ext = [0u8; 8];
                    stream.read_exact(&mut ext).await.unwrap();
                    header_extra = 8;
                    usize::try_from(u64::from_be_bytes(ext)).unwrap()
                }
                other => other as usize,
            };
            let mut payload = vec![0u8; len];
            stream.read_exact(&mut payload).await.unwrap();
            let mut frame = head.to_vec();
            // Rebuild with the extended header in place for header_len().
            if header_extra == 2 {
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            } else if header_extra == 8 {
                frame.extend_from_slice(&(len as u64).to_be_bytes());
            }
            frame.extend_from_slice(&payload);
            frame
        }

        async fn send_ping(&mut self) {
            use tokio::io::AsyncWriteExt;
            // FIN + ping, masked empty payload — RFC 6455 requires the
            // 4-byte masking key even for zero-length frames.
            self.stream
                .write_all(&[0x89, 0x80, 0x11, 0x22, 0x33, 0x44])
                .await
                .unwrap();
            self.stream.flush().await.unwrap();
        }
    }

    fn header_len(frame: &[u8]) -> usize {
        let len7 = frame[1] & 0x7F;
        2 + match len7 {
            126 => 2,
            127 => 8,
            _ => 0,
        }
    }

    #[tokio::test]
    async fn mux_handshake_rejects_bad_token_and_requires_upgrade() {
        let (server, base) = start().await;
        let port = server.port();

        // Bad token → 401 (plain HTTP, no upgrade).
        let client = reqwest::Client::new();
        let response = client
            .get(format!("{base}/api/v1/mux?token=wrong"))
            .header("Upgrade", "websocket")
            .header("Connection", "Upgrade")
            .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401);

        // Good token but no upgrade headers → 400.
        let response = client
            .get(format!("{base}/api/v1/mux?token={}", server.token()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 400);

        // Proper handshake upgrades (WsClient asserts 101 + accept header).
        let _ws = WsClient::connect(port, server.token(), None, None).await;
        server.shutdown();
    }

    #[tokio::test]
    async fn mux_invoke_round_trips_with_id_correlation() {
        let (server, _base) = start().await;
        let mut ws = WsClient::connect(server.port(), server.token(), None, None).await;
        let request = serde_json::to_string(&ProtocolRequest {
            v: 1,
            id: "mux-r1".into(),
            method: "some.echo".into(),
            params: serde_json::json!({}),
        })
        .unwrap();
        ws.send_text(&request).await;
        let text = ws.recv_text().await;
        let response: Response = serde_json::from_str(&text).unwrap();
        assert!(response.ok, "{text}");
        assert_eq!(response.id, "mux-r1");
        assert_eq!(response.result.unwrap()["echo"], "some.echo");
        server.shutdown();
    }

    #[tokio::test]
    async fn mux_carries_live_events_and_replays_after_reconnect() {
        let (server, base) = start().await;
        // Two events land in history BEFORE the mux client connects.
        let _ = post(
            &base,
            "/api/v1/invoke",
            Some(server.token()),
            serde_json::to_vec(&ProtocolRequest {
                v: 1,
                id: "seed-1".into(),
                method: "test.emitSessions".into(),
                params: serde_json::json!({}),
            })
            .unwrap()
            .as_slice(),
        )
        .await;

        // Reconnect from zero: replay both events, then live one.
        let mut ws = WsClient::connect(server.port(), server.token(), Some(0), None).await;
        let first = ws.recv_text().await;
        assert!(first.contains("\"subscription\":\"sess-a\""), "{first}");
        assert!(first.contains("\"global_seq\":1"), "{first}");
        let second = ws.recv_text().await;
        assert!(second.contains("sess-b"), "{second}");

        // A live invoke over HTTP now arrives on the mux stream too.
        let _ = post(
            &base,
            "/api/v1/invoke",
            Some(server.token()),
            serde_json::to_vec(&ProtocolRequest {
                v: 1,
                id: "seed-2".into(),
                method: "session.execute".into(),
                params: serde_json::json!({}),
            })
            .unwrap()
            .as_slice(),
        )
        .await;
        let live = ws.recv_text().await;
        assert!(live.contains("turnStart"), "{live}");
        assert!(live.contains("\"global_seq\":3"), "{live}");
        server.shutdown();
    }

    #[tokio::test]
    async fn mux_session_filter_lets_through_only_matching_envelopes() {
        let (server, base) = start().await;
        let _ = post(
            &base,
            "/api/v1/invoke",
            Some(server.token()),
            serde_json::to_vec(&ProtocolRequest {
                v: 1,
                id: "seed".into(),
                method: "test.emitSessions".into(),
                params: serde_json::json!({}),
            })
            .unwrap()
            .as_slice(),
        )
        .await;
        let mut ws = WsClient::connect(server.port(), server.token(), Some(0), Some("sess-a"))
            .await;
        let only = ws.recv_text().await;
        assert!(only.contains("sess-a"), "{only}");
        assert!(!only.contains("sess-b"), "{only}");
        server.shutdown();
    }

    #[tokio::test]
    async fn mux_ping_gets_pong_and_bad_json_gets_error_response() {
        let (server, _base) = start().await;
        let mut ws = WsClient::connect(server.port(), server.token(), None, None).await;

        // Ping → pong frame (opcode 0xA, empty).
        ws.send_ping().await;
        let frame = ws.recv_raw().await;
        assert_eq!(frame[0] & 0x0F, 0xA, "pong frame: {frame:?}");

        // Bad JSON → an INVALID_PARAMS response that names the raw text.
        ws.send_text("not-json").await;
        let text = ws.recv_text().await;
        let response: Response = serde_json::from_str(&text).unwrap();
        assert!(!response.ok);
        assert_eq!(
            response.error.unwrap().code,
            "INVALID_PARAMS"
        );
        server.shutdown();
    }

    #[tokio::test]
    async fn remote_ui_shell_is_served_and_speaks_the_mux() {
        let (server, base) = start().await;
        let client = reqwest::Client::new();
        let response = client.get(format!("{base}/remote")).send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let body = response.text().await.unwrap();
        assert!(body.contains("/api/v1/mux"), "the shell connects to the mux: {body:?}");
        assert!(body.contains("approval.respond"), "approval decisions ride the mux: {body:?}");
        server.shutdown();
    }
}
