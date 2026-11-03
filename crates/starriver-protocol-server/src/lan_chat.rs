//! LAN chat: peer discovery + device-to-device text/image messaging.
//!
//! Discovery: every instance periodically broadcasts a beacon on a fixed UDP
//! port (41888); a `probe` beacon (sent when the chat panel opens) is answered
//! with a unicast beacon for ~instant discovery. Peers are keyed by a random
//! instance id and named by their hostname; the source IP of the datagram is
//! authoritative (never trusted from the payload).
//!
//! Transport: each instance also listens on `0.0.0.0:<random>` (TCP, minimal
//! HTTP, same hand-rolled style as `http_server`) and receives messages via
//! `POST /lan-chat/v1/message`. Outgoing messages POST the same wire JSON to
//! the peer. The chat port rides the beacon.
//!
//! Persistence: message records (text + image *references*) live in
//! `~/.starriver/lan_chat.json`; image bytes go to a dedicated content-
//! addressed attachment store rooted at `~/.starriver/lan_chat/attachments/`.
//! The store root is deliberately separate from the session attachment store
//! (`~/.starriver/attachments/v1`): chat traffic never touches the session
//! database or the model context — see the isolation tests below.
//!
//! Security model: the LAN is trusted (no auth on the message endpoint — any
//! LAN host may send a message). Abuse is bounded by a body-size cap, a text
//! cap, and a per-IP token bucket; `STARRIVER_LAN_CHAT=0` disables the whole
//! service.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio_util::sync::CancellationToken;

use starriver_host::attachments::{
    AttachmentStore, ImageAttachmentLimits, NormalizationPolicy, SaveImageAttachment,
};
use starriver_protocol::wire::EventEnvelope;

/// Fixed UDP discovery port (unregistered; shared by every instance).
pub const DISCOVERY_PORT: u16 = 41888;
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(3);
const SWEEP_INTERVAL: Duration = Duration::from_secs(5);
/// Peer seen more recently than this is `online`.
const PEER_ONLINE: Duration = Duration::from_secs(12);
/// Peer unseen longer than this drops off the list (messages are kept).
const PEER_EXPIRY: Duration = Duration::from_secs(30);
/// Hard cap on one POST body (carried images included).
const MAX_BODY_BYTES: usize = 6 * 1024 * 1024;
/// Hard cap on one text message.
const MAX_TEXT_BYTES: usize = 8 * 1024;
/// Per-IP token bucket: 30 messages per minute.
const RATE_LIMIT: usize = 30;
const RATE_WINDOW: Duration = Duration::from_secs(60);
/// Persisted messages kept per peer.
const PERSIST_CAP: usize = 200;
/// Outbound POST timeout.
const SEND_TIMEOUT: Duration = Duration::from_secs(3);

// ── Wire / persistence models ──

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    In,
    Out,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Text,
    Image,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Sending,
    Sent,
    Failed,
}

/// Image message metadata: only the content-address reference is stored in
/// the conversation JSON; bytes live in the dedicated attachment store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageMeta {
    #[serde(rename = "ref")]
    pub ref_id: String,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub id: u64,
    pub dir: Dir,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageMeta>,
    pub ts_ms: u64,
    /// Outgoing lifecycle only (`sending` → `sent`/`failed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistModel {
    v: u32,
    conversations: std::collections::BTreeMap<String, Vec<ChatMessage>>,
}

/// Peer identity as seen from beacons.
#[derive(Debug, Clone)]
pub struct Peer {
    pub id: String,
    pub hostname: String,
    pub ip: IpAddr,
    pub port: u16,
    pub last_seen: Instant,
}

/// Incoming message wire format (`POST /lan-chat/v1/message` body).
#[derive(Debug, Deserialize)]
struct IncomingWire {
    v: u8,
    from: WireFrom,
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default, rename = "imageBase64")]
    image_base64: Option<String>,
    #[serde(default)]
    mime: Option<String>,
    #[serde(default, rename = "tsMs")]
    ts_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct WireFrom {
    id: String,
    #[serde(default)]
    hostname: String,
}

/// Delivery refusal: stable code + HTTP status for the chat endpoint.
#[derive(Debug, Clone)]
pub struct DeliverError {
    pub code: &'static str,
    pub status: u16,
    pub message: String,
}

impl DeliverError {
    fn new(code: &'static str, status: u16, message: impl Into<String>) -> Self {
        Self { code, status, message: message.into() }
    }
}

/// Serialize one beacon (announce or probe).
pub fn beacon_json(self_id: &str, hostname: &str, port: u16, probe: bool) -> String {
    let mut beacon = json!({
        "starriver": "chat",
        "proto": 1,
        "id": self_id,
        "hostname": hostname,
        "port": port,
    });
    if probe {
        beacon["probe"] = json!(true);
    }
    beacon.to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    pub id: String,
    pub hostname: String,
    pub port: u16,
    pub probe: bool,
}

/// Parse + validate one beacon datagram. Anything that is not a v1 starriver
/// chat beacon is silently ignored.
pub fn parse_beacon(payload: &str) -> Option<Beacon> {
    let value: Value = serde_json::from_str(payload).ok()?;
    if value.get("starriver")?.as_str()? != "chat" {
        return None;
    }
    if value.get("proto")?.as_u64()? != 1 {
        return None;
    }
    let id = value.get("id")?.as_str()?.to_string();
    if id.is_empty() || id.len() > 128 {
        return None;
    }
    let hostname = value
        .get("hostname")
        .and_then(Value::as_str)
        .unwrap_or("unknown-host")
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect::<String>();
    let port = value.get("port")?.as_u64()?;
    if port == 0 || port > u16::MAX as u64 {
        return None;
    }
    Some(Beacon {
        id,
        hostname,
        port: port as u16,
        probe: value.get("probe").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// This machine's hostname (beacon label + peer list naming).
fn local_hostname() -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

/// Dedicated content-addressed store root for chat images — deliberately
/// separate from the session attachment store (`attachments/v1`).
pub fn default_store() -> Arc<AttachmentStore> {
    let root = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".starriver/lan_chat/attachments"))
        .unwrap_or_else(|_| PathBuf::from(".starriver/lan_chat/attachments"));
    Arc::new(AttachmentStore::new(
        root,
        ImageAttachmentLimits::default(),
        NormalizationPolicy::default(),
    ))
}

pub fn default_store_path() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".starriver/lan_chat.json"))
        .unwrap_or_else(|_| PathBuf::from(".starriver/lan_chat.json"))
}

/// `STARRIVER_LAN_CHAT=0` / `false` / `off` turns the whole service off.
pub fn disabled_by_env() -> bool {
    matches!(
        std::env::var("STARRIVER_LAN_CHAT").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// Broadcast targets: the global broadcast plus every interface's subnet
/// directed broadcast (some managed networks filter the global one).
pub fn broadcast_targets(port: u16) -> Vec<SocketAddr> {
    let mut out = vec![SocketAddr::from(([255, 255, 255, 255], port))];
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            if let if_addrs::IfAddr::V4(v4) = interface.addr {
                let target = v4.broadcast.unwrap_or_else(|| {
                    let ip = u32::from(v4.ip);
                    let mask = u32::from(v4.netmask);
                    Ipv4Addr::from(ip | !mask)
                });
                if !target.is_loopback() && !target.is_unspecified() {
                    out.push(SocketAddr::from((target, port)));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// First non-loopback IPv4 address (display only; peers learn our IP from the
/// datagram source, so this never has to be exact).
pub fn local_lan_ip() -> IpAddr {
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            if let if_addrs::IfAddr::V4(v4) = interface.addr {
                if !v4.ip.is_loopback() && !v4.ip.is_unspecified() {
                    return IpAddr::V4(v4.ip);
                }
            }
        }
    }
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

// ── Service state ──

#[derive(Default)]
struct ChatState {
    peers: HashMap<String, Peer>,
    conversations: HashMap<String, Vec<ChatMessage>>,
    unread: HashMap<String, u32>,
}

/// Knobs for [`ChatService::start`]; defaults are the production values.
#[derive(Clone)]
pub struct ChatOptions {
    /// Event sink (protocol event bus) — incoming messages emit `lanMessage`,
    /// status flips emit `lanMessageUpdated`.
    pub emit: Option<Arc<dyn Fn(EventEnvelope) + Send + Sync>>,
    /// Chat image store. Defaults to the dedicated `lan_chat/attachments` root.
    pub store: Option<Arc<AttachmentStore>>,
    /// Conversation persistence path. Defaults to `~/.starriver/lan_chat.json`.
    pub store_path: Option<PathBuf>,
    /// UDP discovery. Tests disable it and wire peers manually.
    pub discovery: bool,
    pub discovery_port: u16,
}

impl Default for ChatOptions {
    fn default() -> Self {
        Self {
            emit: None,
            store: None,
            store_path: None,
            discovery: true,
            discovery_port: DISCOVERY_PORT,
        }
    }
}

pub struct ChatService {
    self_id: String,
    self_hostname: String,
    chat_port: AtomicU16,
    store: Arc<AttachmentStore>,
    store_path: PathBuf,
    emit: Option<Arc<dyn Fn(EventEnvelope) + Send + Sync>>,
    runtime: tokio::runtime::Handle,
    cancel: CancellationToken,
    state: Mutex<ChatState>,
    rate: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
    next_id: AtomicU64,
    event_seq: AtomicU64,
}

impl ChatService {
    /// Pure constructor (no sockets, no tasks) — production `start` wraps this
    /// with binding + task spawn; tests drive the service directly.
    pub fn new(options: ChatOptions, runtime: tokio::runtime::Handle) -> Arc<Self> {
        let self_id = uuid::Uuid::new_v4().simple().to_string();
        let self_hostname = local_hostname();
        let store = options.store.clone().unwrap_or_else(default_store);
        let store_path = options.store_path.clone().unwrap_or_else(default_store_path);
        let (conversations, max_id) = load_conversations(&store_path);
        Arc::new(Self {
            event_seq: AtomicU64::new(1),
            self_id,
            self_hostname,
            chat_port: AtomicU16::new(0),
            store,
            store_path,
            emit: options.emit,
            runtime,
            cancel: CancellationToken::new(),
            state: Mutex::new(ChatState {
                peers: HashMap::new(),
                conversations,
                unread: HashMap::new(),
            }),
            rate: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(max_id + 1),
        })
    }

    /// Bind sockets and spawn the announce/recv/sweep/listen loops. Also
    /// registers the process-global handle (catalog methods read it).
    pub async fn start(
        options: ChatOptions,
        runtime: tokio::runtime::Handle,
    ) -> anyhow::Result<Arc<Self>> {
        let service = Self::new(options.clone(), runtime.clone());
        let udp = if options.discovery {
            match bind_discovery_socket(options.discovery_port).await {
                Ok(socket) => Some(socket),
                Err(error) => {
                    // Port taken (e.g. a second instance on this host): the
                    // chat itself keeps working, discovery receive degrades.
                    tracing::warn!("lan_chat discovery bind failed: {error}");
                    None
                }
            }
        } else {
            None
        };
        let listener = TcpListener::bind(("0.0.0.0", 0)).await?;
        let port = listener.local_addr()?.port();
        service.chat_port.store(port, Ordering::SeqCst);

        if let Some(previous) = set_global(Some(Arc::clone(&service))) {
            previous.stop();
        }

        // Accept loop for peer messages.
        {
            let service = Arc::clone(&service);
            let cancel = service.cancel.clone();
            let runtime = service.runtime.clone();
            runtime.spawn(async move {
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        accepted = listener.accept() => {
                            let Ok((stream, peer_addr)) = accepted else { continue };
                            let service = Arc::clone(&service);
                            tokio::spawn(async move {
                                let _ = service.handle_chat_connection(stream, peer_addr.ip()).await;
                            });
                        }
                    }
                }
            });
        }

        if let Some(socket) = udp {
            // One loop owns the socket: periodic announce + probe replies +
            // beacon receive (tokio UdpSocket is neither Clone nor try_clone).
            {
                let service = Arc::clone(&service);
                let cancel = service.cancel.clone();
                let runtime = service.runtime.clone();
                runtime.spawn(async move {
                    let beacon = service.beacon(false);
                    let probe = service.beacon(true);
                    // Immediate announce + probe so boot-time discovery is fast.
                    send_beacon_all(&socket, &beacon).await;
                    send_beacon_all(&socket, &probe).await;
                    let mut tick = tokio::time::interval(ANNOUNCE_INTERVAL);
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    let mut buf = vec![0u8; 4096];
                    loop {
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            _ = tick.tick() => send_beacon_all(&socket, &beacon).await,
                            received = socket.recv_from(&mut buf) => {
                                let Ok((n, src)) = received else { continue };
                                let Some(beacon) =
                                    parse_beacon(&String::from_utf8_lossy(&buf[..n]))
                                else {
                                    continue;
                                };
                                if beacon.id == service.self_id {
                                    continue; // our own broadcast
                                }
                                service.upsert_peer(
                                    &beacon.id,
                                    &beacon.hostname,
                                    src.ip(),
                                    beacon.port,
                                );
                                if beacon.probe {
                                    let reply = service.beacon(false);
                                    let _ = socket.send_to(reply.as_bytes(), src).await;
                                }
                            }
                        }
                    }
                });
            }
        }

        // Peer expiry sweeper (list only; conversations are kept forever).
        {
            let service = Arc::clone(&service);
            let cancel = service.cancel.clone();
            let runtime = service.runtime.clone();
            runtime.spawn(async move {
                let mut tick = tokio::time::interval(SWEEP_INTERVAL);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tick.tick() => service.sweep_expired(),
                    }
                }
            });
        }

        Ok(service)
    }

    pub fn stop(&self) {
        self.cancel.cancel();
        clear_global_if(self);
    }

    pub fn self_id(&self) -> &str {
        &self.self_id
    }

    pub fn self_hostname(&self) -> &str {
        &self.self_hostname
    }

    /// TCP port peers should POST messages to (advertised in the beacon).
    pub fn chat_port(&self) -> u16 {
        self.chat_port.load(Ordering::SeqCst)
    }

    /// Broadcast a probe beacon (called when the chat panel opens/mark-read)
    /// so currently-silent instances surface immediately. Sync wrapper: safe
    /// to call from catalog handlers.
    pub fn request_probe(self: &Arc<Self>) {
        let service = Arc::clone(self);
        self.runtime.spawn(async move {
            service.probe_peers().await;
        });
    }

    /// Broadcast a probe beacon once (async core of [`request_probe`]).
    pub async fn probe_peers(&self) {
        let Ok(socket) = bind_discovery_socket(0).await else { return };
        let probe = self.beacon(true);
        for target in broadcast_targets(DISCOVERY_PORT) {
            let _ = socket.send_to(probe.as_bytes(), target).await;
        }
    }

    fn beacon(&self, probe: bool) -> String {
        beacon_json(&self.self_id, &self.self_hostname, self.chat_port(), probe)
    }

    // ── Peer bookkeeping ──

    pub fn upsert_peer(&self, id: &str, hostname: &str, ip: IpAddr, port: u16) {
        if id == self.self_id || id.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let entry = state.peers.entry(id.to_string()).or_insert_with(|| Peer {
            id: id.to_string(),
            hostname: String::new(),
            ip,
            port,
            last_seen: Instant::now(),
        });
        entry.hostname = hostname.to_string();
        entry.ip = ip;
        entry.port = port;
        entry.last_seen = Instant::now();
    }

    /// Test hook: wire a peer without discovery.
    pub fn insert_peer_for_test(&self, id: &str, hostname: &str, ip: IpAddr, port: u16) {
        self.upsert_peer(id, hostname, ip, port);
    }

    fn sweep_expired(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let before = state.peers.len();
        state.peers.retain(|_, peer| peer.last_seen.elapsed() < PEER_EXPIRY);
        if state.peers.len() != before {
            tracing::debug!("lan_chat: expired {} stale peers", before - state.peers.len());
        }
    }

    // ── Incoming delivery (chat endpoint + tests) ──

    fn allow_rate(&self, ip: IpAddr) -> bool {
        let mut rate = self.rate.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let window = rate.entry(ip).or_default();
        window.retain(|at| now.duration_since(*at) < RATE_WINDOW);
        if window.len() >= RATE_LIMIT {
            return false;
        }
        window.push_back(now);
        true
    }

    /// Validate + store one incoming message. Shared by the TCP endpoint and
    /// the tests; `src_ip` only feeds the rate limiter.
    pub async fn deliver_incoming(
        &self,
        from: WireFrom,
        kind: String,
        text: Option<String>,
        image_base64: Option<String>,
        mime: Option<String>,
        ts_ms: Option<u64>,
        src_ip: IpAddr,
    ) -> Result<u64, DeliverError> {
        if !self.allow_rate(src_ip) {
            return Err(DeliverError::new("RATE_LIMITED", 429, "too many messages"));
        }
        if from.id.is_empty() || from.id.len() > 128 {
            return Err(DeliverError::new("INVALID_PARAMS", 400, "from.id is required"));
        }
        let kind = match kind.as_str() {
            "text" => Kind::Text,
            "image" => Kind::Image,
            _ => return Err(DeliverError::new("INVALID_PARAMS", 400, "kind must be text|image")),
        };
        let ts = ts_ms.unwrap_or_else(now_ms);
        let (text, image) = match kind {
            Kind::Text => {
                let text = text.unwrap_or_default();
                if text.is_empty() {
                    return Err(DeliverError::new("INVALID_PARAMS", 400, "text is required"));
                }
                if text.len() > MAX_TEXT_BYTES {
                    return Err(DeliverError::new("TEXT_TOO_LARGE", 413, "text exceeds 8 KiB"));
                }
                (Some(text), None)
            }
            Kind::Image => {
                let raw = image_base64.ok_or_else(|| {
                    DeliverError::new("INVALID_PARAMS", 400, "imageBase64 is required")
                })?;
                let mime = mime.unwrap_or_else(|| "image/png".to_string());
                if !matches!(mime.as_str(), "image/png" | "image/jpeg" | "image/webp") {
                    return Err(DeliverError::new(
                        "INVALID_PARAMS",
                        400,
                        "mime must be image/png|jpeg|webp",
                    ));
                }
                let data = base64::engine::general_purpose::STANDARD
                    .decode(raw.trim())
                    .map_err(|_| {
                        DeliverError::new("INVALID_PARAMS", 400, "imageBase64 is not valid base64")
                    })?;
                let image =
                    self.store_image(data, mime).await.map_err(|message| {
                        DeliverError::new("IMAGE_TOO_LARGE", 413, message)
                    })?;
                (None, Some(image))
            }
        };

        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let message =
            ChatMessage { id, dir: Dir::In, kind, text, image, ts_ms: ts, status: None };
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let conversation = state.conversations.entry(from.id.clone()).or_default();
            conversation.push(message.clone());
            if conversation.len() > PERSIST_CAP {
                let overflow = conversation.len() - PERSIST_CAP;
                conversation.drain(0..overflow);
            }
            *state.unread.entry(from.id.clone()).or_insert(0) += 1;
            self.persist_locked(&state);
        }
        self.emit_event(
            "lanMessage",
            json!({ "peerId": from.id, "message": serde_json::to_value(&message).unwrap_or_default() }),
        );
        Ok(id)
    }

    /// Normalize + commit image bytes into the dedicated chat store.
    async fn store_image(&self, data: Vec<u8>, mime: String) -> Result<ImageMeta, String> {
        let store = self.store.clone();
        let name = format!("lan-chat-{}", now_ms());
        let saved = tokio::task::spawn_blocking(move || {
            store.save_images(&[SaveImageAttachment {
                name: Some(name),
                media_type: mime,
                data,
            }])
        })
        .await
        .map_err(|e| format!("image store task failed: {e}"))?;
        let reference = saved
            .map_err(|e| match e.code {
                starriver_host::attachments::AttachmentErrorCode::ImageTooLarge
                | starriver_host::attachments::AttachmentErrorCode::ImageBatchTooLarge => {
                    format!("image exceeds store limits ({})", e.message)
                }
                _ => format!("image store failed: {}", e.message),
            })?
            .into_iter()
            .next()
            .ok_or_else(|| "image store returned no reference".to_string())?;
        Ok(ImageMeta {
            ref_id: reference.attachment_id,
            mime: reference.media_type,
            width: reference.width,
            height: reference.height,
            bytes: reference.bytes as u64,
        })
    }

    fn emit_event(&self, event_type: &str, payload: Value) {
        if let Some(emit) = &self.emit {
            let seq = self.event_seq.fetch_add(1, Ordering::SeqCst);
            let envelope = EventEnvelope::new("lanChat", seq, event_type, payload);
            emit(envelope);
        }
    }

    // ── Outgoing ──

    /// Append an outgoing record (status `sending`) and POST it to the peer
    /// asynchronously. Returns the record id immediately — the catalog handler
    /// must never block the JS thread on network I/O.
    pub fn send(
        self: &Arc<Self>,
        peer_id: &str,
        kind: String,
        text: Option<String>,
        image_base64: Option<String>,
        mime: Option<String>,
    ) -> Result<u64, DeliverError> {
        let peer = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.peers.get(peer_id).map(|p| (p.ip, p.port)).ok_or_else(|| {
                DeliverError::new("PEER_OFFLINE", 404, format!("unknown peer `{peer_id}`"))
            })?
        };
        let kind_enum = match kind.as_str() {
            "text" => Kind::Text,
            "image" => Kind::Image,
            _ => return Err(DeliverError::new("INVALID_PARAMS", 400, "kind must be text|image")),
        };

        // Build the wire payload (validates the payload synchronously so
        // errors surface to the caller, not to a detached task).
        let mut wire = json!({
            "v": 1,
            "from": { "id": self.self_id, "hostname": self.self_hostname },
            "kind": kind,
            "tsMs": now_ms(),
        });
        match kind_enum {
            Kind::Text => {
                let text = text.unwrap_or_default();
                if text.is_empty() {
                    return Err(DeliverError::new("INVALID_PARAMS", 400, "text is required"));
                }
                if text.len() > MAX_TEXT_BYTES {
                    return Err(DeliverError::new("TEXT_TOO_LARGE", 413, "text exceeds 8 KiB"));
                }
                wire["text"] = json!(text);
            }
            Kind::Image => {
                let raw = image_base64.ok_or_else(|| {
                    DeliverError::new("INVALID_PARAMS", 400, "imageBase64 is required")
                })?;
                let mime = mime.unwrap_or_else(|| "image/png".to_string());
                if !matches!(mime.as_str(), "image/png" | "image/jpeg" | "image/webp") {
                    return Err(DeliverError::new(
                        "INVALID_PARAMS",
                        400,
                        "mime must be image/png|jpeg|webp",
                    ));
                }
                wire["mime"] = json!(mime);
                wire["imageBase64"] = json!(raw);
            }
        }
        let ts = wire["tsMs"].as_u64().unwrap_or_else(now_ms);

        // Reserve the record synchronously so ordering is stable; the spawned
        // task fills in the image ref and flips the status afterwards.
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .conversations
                .entry(peer_id.to_string())
                .or_default()
                .push(ChatMessage {
                    id,
                    dir: Dir::Out,
                    kind: kind_enum.clone(),
                    text: wire["text"].as_str().map(str::to_string),
                    image: None,
                    ts_ms: ts,
                    status: Some(Status::Sending),
                });
            self.persist_locked(&state);
        }

        let service_image = (kind_enum == Kind::Image).then(|| {
            (
                wire["imageBase64"].as_str().unwrap_or_default().to_string(),
                wire["mime"].as_str().unwrap_or("image/png").to_string(),
            )
        });
        let peer_id_owned = peer_id.to_string();
        let task_wire = wire.clone();
        let service = Arc::clone(self);
        self.runtime.spawn(async move {
            // Store the sender's own copy so the sent history survives restart.
            let image_meta = match service_image {
                Some((raw, mime)) => {
                    let data = match base64::engine::general_purpose::STANDARD.decode(raw.trim()) {
                        Ok(data) => data,
                        Err(_) => {
                            service.mark_status(&peer_id_owned, id, Status::Failed);
                            return;
                        }
                    };
                    match service.store_image(data, mime).await {
                        Ok(meta) => Some(meta),
                        Err(message) => {
                            tracing::warn!("lan_chat send image store failed: {message}");
                            service.mark_status(&peer_id_owned, id, Status::Failed);
                            return;
                        }
                    }
                }
                None => None,
            };
            if let Some(meta) = &image_meta {
                let mut state = service.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(message) = state
                    .conversations
                    .get_mut(&peer_id_owned)
                    .and_then(|list| list.iter_mut().find(|m| m.id == id))
                {
                    message.image = Some(meta.clone());
                }
                service.persist_locked(&state);
            }
            // The wire copy carries the ORIGINAL bytes; the peer normalizes on
            // its side (same deterministic policy → same content address).
            let delivered = post_message(SocketAddr::new(peer.0, peer.1), &task_wire).await.is_ok();
            service.mark_status(
                &peer_id_owned,
                id,
                if delivered { Status::Sent } else { Status::Failed },
            );
        });
        Ok(id)
    }

    fn mark_status(&self, peer_id: &str, id: u64, status: Status) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(message) = state
                .conversations
                .get_mut(peer_id)
                .and_then(|list| list.iter_mut().find(|m| m.id == id))
            {
                message.status = Some(status);
            }
            self.persist_locked(&state);
        }
        self.emit_event(
            "lanMessageUpdated",
            json!({ "peerId": peer_id, "id": id, "status": status }),
        );
    }

    // ── Catalog-facing JSON ──

    pub fn state_json(&self) -> Value {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let peer_json = |peer: &Peer| {
            json!({
                "id": peer.id,
                "hostname": peer.hostname,
                "ip": peer.ip.to_string(),
                "port": peer.port,
                "lastSeenMs": now_ms().saturating_sub(peer.last_seen.elapsed().as_millis() as u64),
                "online": peer.last_seen.elapsed() < PEER_ONLINE,
            })
        };
        let mut peers: Vec<Value> = state.peers.values().map(&peer_json).collect();
        peers.sort_by(|a, b| {
            let host =
                a["hostname"].as_str().unwrap_or("").cmp(b["hostname"].as_str().unwrap_or(""));
            host.then_with(|| a["id"].as_str().unwrap_or("").cmp(b["id"].as_str().unwrap_or("")))
        });
        // Conversations may exist for peers whose beacon expired (or senders
        // never seen on this boot) — the UI merges these with `peers`.
        let mut conversations: Vec<String> = state.conversations.keys().cloned().collect();
        conversations.sort();
        let mut unread = Map::new();
        for (id, count) in &state.unread {
            if *count > 0 {
                unread.insert(id.clone(), json!(count));
            }
        }
        let unread_total: u32 = state.unread.values().sum();
        json!({
            "enabled": true,
            "self": {
                "id": self.self_id,
                "hostname": self.self_hostname,
                "port": self.chat_port(),
                "ip": local_lan_ip().to_string(),
            },
            "peers": peers,
            "conversations": conversations,
            "unread": unread,
            "unreadTotal": unread_total,
        })
    }

    pub fn messages_json(&self, peer_id: &str, since_id: Option<u64>) -> Value {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let messages = state
            .conversations
            .get(peer_id)
            .map(|list| {
                list.iter()
                    .filter(|message| since_id.is_none_or(|since| message.id > since))
                    .filter_map(|message| serde_json::to_value(message).ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let peer = state.peers.get(peer_id).map(|peer| {
            json!({
                "id": peer.id,
                "hostname": peer.hostname,
                "ip": peer.ip.to_string(),
                "port": peer.port,
                "lastSeenMs": now_ms().saturating_sub(peer.last_seen.elapsed().as_millis() as u64),
                "online": peer.last_seen.elapsed() < PEER_ONLINE,
            })
        });
        json!({ "messages": messages, "peer": peer })
    }

    pub fn mark_read(&self, peer_id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.unread.remove(peer_id);
    }

    /// Read one chat image back for rendering (`sha256:` refs only, and only
    /// from the dedicated chat store — never the session attachment root).
    pub fn attachment_json(&self, id: &str) -> Option<Value> {
        let stored = self.store.read_image_light(id)?;
        Some(json!({
            "id": stored.reference.attachment_id,
            "base64": base64::engine::general_purpose::STANDARD.encode(&stored.data),
            "mime": stored.reference.media_type,
            "width": stored.reference.width,
            "height": stored.reference.height,
            "bytes": stored.reference.bytes,
        }))
    }

    // ── Chat TCP endpoint ──

    async fn handle_chat_connection(
        &self,
        mut stream: TcpStream,
        src_ip: IpAddr,
    ) -> anyhow::Result<()> {
        let request = match crate::http_server::read_request(&mut stream).await {
            Ok(request) => request,
            Err(error) => {
                crate::http_server::write_response(
                    &mut stream,
                    400,
                    "application/json",
                    format!(
                        r#"{{"ok":false,"error":{{"code":"BAD_REQUEST","message":"{error}"}}}}"#
                    )
                    .as_bytes(),
                )
                .await?;
                return Ok(());
            }
        };
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/lan-chat/v1/ping") => {
                crate::http_server::write_response(
                    &mut stream,
                    200,
                    "application/json",
                    br#"{"ok":true,"service":"lan-chat","v":1}"#,
                )
                .await?;
            }
            ("POST", "/lan-chat/v1/message") => {
                if request.body.len() > MAX_BODY_BYTES {
                    crate::http_server::write_response(
                        &mut stream,
                        413,
                        "application/json",
                        br#"{"ok":false,"error":{"code":"BODY_TOO_LARGE","message":"body exceeds 6 MiB"}}"#,
                    )
                    .await?;
                    return Ok(());
                }
                let wire: IncomingWire = match serde_json::from_slice::<IncomingWire>(&request.body) {
                    Ok(wire) if wire.v == 1 => wire,
                    _ => {
                        crate::http_server::write_response(
                            &mut stream,
                            400,
                            "application/json",
                            br#"{"ok":false,"error":{"code":"INVALID_PARAMS","message":"bad message JSON (expect v1 chat envelope)"}}"#,
                        )
                        .await?;
                        return Ok(());
                    }
                };
                let result = self
                    .deliver_incoming(
                        wire.from,
                        wire.kind,
                        wire.text,
                        wire.image_base64,
                        wire.mime,
                        wire.ts_ms,
                        src_ip,
                    )
                    .await;
                let (status, body) = match result {
                    Ok(id) => (200, json!({ "ok": true, "id": id }).to_string()),
                    Err(error) => (
                        error.status,
                        json!({ "ok": false, "error": { "code": error.code, "message": error.message } })
                            .to_string(),
                    ),
                };
                crate::http_server::write_response(
                    &mut stream,
                    status,
                    "application/json",
                    body.as_bytes(),
                )
                .await?;
            }
            _ => {
                crate::http_server::write_response(
                    &mut stream,
                    404,
                    "application/json",
                    br#"{"ok":false,"error":{"code":"NOT_FOUND","message":"unknown route"}}"#,
                )
                .await?;
            }
        }
        Ok(())
    }

    // ── Persistence ──

    /// Serialize + atomically write the conversation file. Callers hold the
    /// state lock; the write is small (refs only, 200 records per peer max).
    fn persist_locked(&self, state: &ChatState) {
        let model = PersistModel {
            v: 1,
            conversations: state
                .conversations
                .iter()
                .map(|(id, list)| (id.clone(), list.clone()))
                .collect(),
        };
        let payload = match serde_json::to_string(&model) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!("lan_chat persist encode failed: {error}");
                return;
            }
        };
        if let Some(parent) = self.store_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.store_path.with_extension("json.tmp");
        if std::fs::write(&tmp, payload).is_ok() {
            let _ = std::fs::rename(&tmp, &self.store_path);
        }
    }
}

/// One minimal HTTP POST with a hard timeout; Ok only on HTTP 200.
async fn post_message(addr: SocketAddr, body: &Value) -> Result<(), String> {
    let payload = body.to_string();
    let attempt = async {
        let mut stream =
            TcpStream::connect(addr).await.map_err(|e| format!("connect {addr}: {e}"))?;
        let head = format!(
            "POST /lan-chat/v1/message HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len(),
        );
        stream.write_all(head.as_bytes()).await.map_err(|e| format!("write: {e}"))?;
        stream.write_all(payload.as_bytes()).await.map_err(|e| format!("write: {e}"))?;
        let mut buf = vec![0u8; 1024];
        let n = stream.read(&mut buf).await.map_err(|e| format!("read: {e}"))?;
        let head = String::from_utf8_lossy(&buf[..n]);
        let status = head.split_whitespace().nth(1).unwrap_or("0");
        if status == "200" {
            Ok(())
        } else {
            Err(format!("peer responded HTTP {status}"))
        }
    };
    tokio::time::timeout(SEND_TIMEOUT, attempt)
        .await
        .map_err(|_| "send timed out".to_string())?
}

async fn bind_discovery_socket(port: u16) -> anyhow::Result<UdpSocket> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    socket.set_reuse_address(true)?;
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&socket2::SockAddr::from(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))))?;
    let std_socket: std::net::UdpSocket = socket.into();
    Ok(UdpSocket::from_std(std_socket)?)
}

async fn send_beacon_all(socket: &UdpSocket, beacon: &str) {
    for target in broadcast_targets(DISCOVERY_PORT) {
        let _ = socket.send_to(beacon.as_bytes(), target).await;
    }
}

// ── Process-global handle (catalog access) ──

static GLOBAL: Mutex<Option<Arc<ChatService>>> = Mutex::new(None);

pub fn global() -> Option<Arc<ChatService>> {
    GLOBAL.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set_global(service: Option<Arc<ChatService>>) -> Option<Arc<ChatService>> {
    std::mem::replace(&mut *GLOBAL.lock().unwrap_or_else(|e| e.into_inner()), service)
}

fn clear_global_if(service: &ChatService) {
    let matches = GLOBAL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|s| s.self_id == service.self_id)
        .unwrap_or(false);
    if matches {
        *GLOBAL.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

// ── Persistence load ──

fn load_conversations(path: &std::path::Path) -> (HashMap<String, Vec<ChatMessage>>, u64) {
    let Ok(text) = std::fs::read_to_string(path) else { return (HashMap::new(), 0) };
    let Ok(model) = serde_json::from_str::<PersistModel>(&text) else {
        tracing::warn!("lan_chat store {} is corrupt — starting empty", path.display());
        return (HashMap::new(), 0);
    };
    let mut max_id = 0u64;
    let conversations = model
        .conversations
        .into_iter()
        .map(|(id, mut list)| {
            list.sort_by_key(|m| m.id);
            list.truncate(PERSIST_CAP);
            if let Some(last) = list.last() {
                max_id = max_id.max(last.id);
            }
            (id, list)
        })
        .collect();
    (conversations, max_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_host::attachments::types::{ImageAttachmentLimits, NormalizationPolicy};

    fn test_service(
        dir: &std::path::Path,
        emit: Option<Arc<dyn Fn(EventEnvelope) + Send + Sync>>,
    ) -> Arc<ChatService> {
        let store = Arc::new(AttachmentStore::new(
            dir.join("attachments"),
            ImageAttachmentLimits::default(),
            NormalizationPolicy::default(),
        ));
        ChatService::new(
            ChatOptions {
                emit,
                store: Some(store),
                store_path: Some(dir.join("lan_chat.json")),
                discovery: false,
                discovery_port: 0,
            },
            tokio::runtime::Handle::current(),
        )
    }

    /// Spawn a real chat endpoint for `service` on an ephemeral loopback port.
    async fn serve_on_loopback(service: Arc<ChatService>) -> u16 {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((stream, addr)) = listener.accept().await else { break };
                let svc = Arc::clone(&service);
                tokio::spawn(async move {
                    let _ = svc.handle_chat_connection(stream, addr.ip()).await;
                });
            }
        });
        port
    }

    fn from(id: &str) -> WireFrom {
        WireFrom { id: id.to_string(), hostname: format!("host-{id}") }
    }

    /// 1×1 PNG (67 bytes) — valid for the store's sniff+decode path.
    fn tiny_png() -> Vec<u8> {
        const PNG: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
            0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        PNG.to_vec()
    }

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    /// Generate a real PNG of the given size (distinct sizes → distinct
    /// normalized bytes → distinct content addresses).
    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let img = image::DynamicImage::new_rgb8(width, height);
        let mut buffer = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buffer, image::ImageFormat::Png).unwrap();
        buffer.into_inner()
    }

    #[test]
    fn beacon_round_trips_and_rejects_foreign_payloads() {
        let beacon = beacon_json("abc123", "macbook", 51234, false);
        let parsed = parse_beacon(&beacon).expect("valid beacon parses");
        assert_eq!(parsed.id, "abc123");
        assert_eq!(parsed.hostname, "macbook");
        assert_eq!(parsed.port, 51234);
        assert!(!parsed.probe);

        let probe = parse_beacon(&beacon_json("abc123", "macbook", 51234, true)).expect("probe");
        assert!(probe.probe);

        assert!(parse_beacon("not json").is_none());
        assert!(parse_beacon(r#"{"starriver":"other","proto":1,"id":"x","port":1}"#).is_none());
        assert!(parse_beacon(r#"{"starriver":"chat","proto":2,"id":"x","port":1}"#).is_none());
        assert!(parse_beacon(r#"{"starriver":"chat","proto":1,"id":"x"}"#).is_none());
        assert!(parse_beacon(r#"{"starriver":"chat","proto":1,"id":"x","port":0}"#).is_none());
        assert!(parse_beacon(r#"{"starriver":"chat","proto":1,"id":"x","port":99999}"#).is_none());
    }

    #[tokio::test]
    async fn text_delivery_updates_store_unread_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let service = test_service(
            dir.path(),
            Some(Arc::new(move |envelope: EventEnvelope| {
                sink.lock().unwrap().push(envelope.event_type);
            })),
        );

        let id = service
            .deliver_incoming(
                from("peer-1"),
                "text".into(),
                Some("hello".into()),
                None,
                None,
                None,
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            )
            .await
            .expect("text delivered");
        assert_eq!(id, 1);

        let snapshot = service.messages_json("peer-1", None);
        let messages = snapshot["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["dir"], "in");
        assert_eq!(messages[0]["kind"], "text");
        assert_eq!(messages[0]["text"], "hello");

        let state = service.state_json();
        assert_eq!(state["unreadTotal"], 1);
        assert_eq!(state["unread"]["peer-1"], 1);
        // Conversation keys surface even without a live peer beacon.
        assert_eq!(state["conversations"].as_array().unwrap().len(), 1);

        service.mark_read("peer-1");
        assert_eq!(service.state_json()["unreadTotal"], 0);

        // sinceId increment: nothing new after the last id.
        let empty = service.messages_json("peer-1", Some(id));
        assert_eq!(empty["messages"].as_array().unwrap().len(), 0);

        assert_eq!(events.lock().unwrap().as_slice(), ["lanMessage"]);

        // Restart: same store path reloads the conversation.
        let reloaded = test_service(dir.path(), None);
        assert_eq!(
            reloaded.messages_json("peer-1", None)["messages"].as_array().unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn text_delivery_rejects_bad_input() {
        let dir = tempfile::tempdir().unwrap();
        let service = test_service(dir.path(), None);

        let empty = service
            .deliver_incoming(
                from("p"),
                "text".into(),
                Some(String::new()),
                None,
                None,
                None,
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            )
            .await
            .unwrap_err();
        assert_eq!(empty.code, "INVALID_PARAMS");

        let long = "x".repeat(MAX_TEXT_BYTES + 1);
        let too_long = service
            .deliver_incoming(
                from("p"),
                "text".into(),
                Some(long),
                None,
                None,
                None,
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            )
            .await
            .unwrap_err();
        assert_eq!(too_long.code, "TEXT_TOO_LARGE");
        assert_eq!(too_long.status, 413);

        let bad_kind = service
            .deliver_incoming(
                from("p"),
                "voice".into(),
                Some("hi".into()),
                None,
                None,
                None,
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            )
            .await
            .unwrap_err();
        assert_eq!(bad_kind.code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn image_delivery_lands_in_dedicated_store_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let session_store_dir = tempfile::tempdir().unwrap();
        // Simulate the session attachment store in a separate root.
        let session_store = Arc::new(AttachmentStore::new(
            session_store_dir.path().join("attachments"),
            ImageAttachmentLimits::default(),
            NormalizationPolicy::default(),
        ));
        let session_refs = session_store
            .save_images(&[SaveImageAttachment {
                name: Some("session.png".into()),
                media_type: "image/png".into(),
                // Different dimensions than the chat image below → different
                // normalized bytes → a genuinely distinct content address.
                data: png_bytes(3, 2),
            }])
            .unwrap();
        let session_ref = session_refs[0].attachment_id.clone();

        let service = test_service(dir.path(), None);
        let png = tiny_png();
        service
            .deliver_incoming(
                from("peer-1"),
                "image".into(),
                None,
                Some(b64(&png)),
                Some("image/png".into()),
                None,
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            )
            .await
            .expect("image delivered");

        let messages =
            service.messages_json("peer-1", None)["messages"].as_array().unwrap().clone();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["kind"], "image");
        let ref_id = messages[0]["image"]["ref"].as_str().unwrap().to_string();
        assert!(ref_id.starts_with("sha256:"));

        // The store normalizes on admission (the 1×1 PNG may be re-encoded),
        // so assert store round-trip semantics instead of byte identity: the
        // ref resolves, carries a raster mime, and decodes to real image bytes.
        let attachment = service.attachment_json(&ref_id).expect("chat image readable");
        let mime = attachment["mime"].as_str().unwrap().to_string();
        assert!(matches!(mime.as_str(), "image/png" | "image/jpeg" | "image/webp"));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(attachment["base64"].as_str().unwrap())
            .unwrap();
        assert!(
            decoded.starts_with(&[0x89, b'P', b'N', b'G'])
                || decoded.starts_with(&[0xFF, 0xD8])
                || (decoded.len() >= 12 && &decoded[0..4] == b"RIFF" && &decoded[8..12] == b"WEBP"),
            "stored chat image is not a recognizable raster image"
        );
        assert_eq!(attachment["width"], 1);
        assert_eq!(attachment["height"], 1);

        // Restart: history (with the ref) reloads and the image still resolves.
        let reloaded = test_service(dir.path(), None);
        let messages =
            reloaded.messages_json("peer-1", None)["messages"].as_array().unwrap().clone();
        assert_eq!(messages[0]["image"]["ref"].as_str(), Some(ref_id.as_str()));
        assert!(reloaded.attachment_json(&ref_id).is_some());

        // Isolation: the chat store never sees session attachments (distinct
        // content), and each root holds exactly its own one object — chat
        // traffic added nothing to the session root.
        assert!(reloaded.attachment_json(&session_ref).is_none());
        assert_eq!(count_objects(session_store_dir.path().join("attachments")), 1);
        assert_eq!(count_objects(dir.path().join("attachments")), 1);
    }

    /// Count content-addressed object files under a store root.
    fn count_objects(root: impl AsRef<std::path::Path>) -> usize {
        let mut count = 0;
        let buckets = match std::fs::read_dir(root.as_ref().join("objects")) {
            Ok(entries) => entries,
            Err(_) => return 0,
        };
        for bucket in buckets.flatten() {
            if let Ok(objects) = std::fs::read_dir(bucket.path()) {
                count += objects.count();
            }
        }
        count
    }

    #[tokio::test]
    async fn image_delivery_rejects_garbage_and_missing_payload() {
        let dir = tempfile::tempdir().unwrap();
        let service = test_service(dir.path(), None);

        let missing = service
            .deliver_incoming(
                from("p"),
                "image".into(),
                None,
                None,
                None,
                None,
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            )
            .await
            .unwrap_err();
        assert_eq!(missing.code, "INVALID_PARAMS");

        let not_b64 = service
            .deliver_incoming(
                from("p"),
                "image".into(),
                None,
                Some("!!!".into()),
                Some("image/png".into()),
                None,
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            )
            .await
            .unwrap_err();
        assert_eq!(not_b64.code, "INVALID_PARAMS");

        let bad_mime = service
            .deliver_incoming(
                from("p"),
                "image".into(),
                None,
                Some(b64(&tiny_png())),
                Some("image/gif".into()),
                None,
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            )
            .await
            .unwrap_err();
        assert_eq!(bad_mime.code, "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn rate_limit_caps_burst_per_ip() {
        let dir = tempfile::tempdir().unwrap();
        let service = test_service(dir.path(), None);
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 7, 7));
        for _ in 0..RATE_LIMIT {
            service
                .deliver_incoming(
                    from("p"),
                    "text".into(),
                    Some("m".into()),
                    None,
                    None,
                    None,
                    ip,
                )
                .await
                .expect("within rate limit");
        }
        let refused = service
            .deliver_incoming(from("p"), "text".into(), Some("m".into()), None, None, None, ip)
            .await
            .unwrap_err();
        assert_eq!(refused.code, "RATE_LIMITED");
        assert_eq!(refused.status, 429);
        // A different source is unaffected.
        assert!(service
            .deliver_incoming(
                from("p"),
                "text".into(),
                Some("m".into()),
                None,
                None,
                None,
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            )
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn peer_state_reports_online_and_expiry_list_removal() {
        let dir = tempfile::tempdir().unwrap();
        let service = test_service(dir.path(), None);
        service.insert_peer_for_test(
            "peer-1",
            "dev-linux",
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 23)),
            55555,
        );
        let state = service.state_json();
        assert_eq!(state["enabled"], true);
        assert_eq!(state["self"]["hostname"], service.self_hostname());
        let peers = state["peers"].as_array().unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0]["hostname"], "dev-linux");
        assert_eq!(peers[0]["online"], true);

        // Expire the peer: drops from the list, conversation stays.
        {
            let mut state = service.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(peer) = state.peers.get_mut("peer-1") {
                peer.last_seen = Instant::now() - PEER_EXPIRY - Duration::from_secs(1);
            }
        }
        service.sweep_expired();
        assert_eq!(service.state_json()["peers"].as_array().unwrap().len(), 0);
        assert_eq!(service.messages_json("peer-1", None)["peer"], Value::Null);
    }

    /// Loopback integration: A and B each run the real chat endpoint and
    /// exchange text and an image over HTTP.
    #[tokio::test]
    async fn loopback_pair_exchanges_text_and_image() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let events_b: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events_b.clone();
        let service_a = test_service(dir_a.path(), None);
        let service_b = test_service(
            dir_b.path(),
            Some(Arc::new(move |envelope: EventEnvelope| {
                sink.lock().unwrap().push(envelope.event_type);
            })),
        );
        let port_a = serve_on_loopback(Arc::clone(&service_a)).await;
        let port_b = serve_on_loopback(Arc::clone(&service_b)).await;
        let id_a = service_a.self_id().to_string();
        let id_b = service_b.self_id().to_string();

        service_a.insert_peer_for_test(&id_b, "dev-linux", IpAddr::V4(Ipv4Addr::LOCALHOST), port_b);
        service_b.insert_peer_for_test(&id_a, "host-a", IpAddr::V4(Ipv4Addr::LOCALHOST), port_a);

        // Text A → B.
        service_a.send(&id_b, "text".into(), Some("ping".into()), None, None).unwrap();
        let mut delivered = false;
        for _ in 0..150 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let messages = service_b.messages_json(&id_a, None)["messages"].as_array().unwrap().clone();
            if !messages.is_empty() {
                assert_eq!(messages[0]["text"], "ping");
                assert_eq!(messages[0]["dir"], "in");
                delivered = true;
                break;
            }
        }
        assert!(delivered, "B never received A's text");
        assert!(events_b.lock().unwrap().iter().any(|kind| kind == "lanMessage"));

        // A's record flips to sent (async POST settled).
        let mut sent = false;
        for _ in 0..150 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mine = service_a.messages_json(&id_b, None)["messages"].as_array().unwrap().clone();
            if mine[0]["status"] == "sent" {
                sent = true;
                break;
            }
        }
        assert!(sent, "A's text record never flipped to sent");

        // Image A → B over the same channel.
        service_a
            .send(&id_b, "image".into(), None, Some(b64(&tiny_png())), Some("image/png".into()))
            .unwrap();
        let mut image_arrived = false;
        for _ in 0..150 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let messages = service_b.messages_json(&id_a, None)["messages"].as_array().unwrap().clone();
            if messages.len() == 2 && messages[1]["kind"] == "image" {
                image_arrived = true;
                break;
            }
        }
        assert!(image_arrived, "B never received A's image");
    }

    #[tokio::test]
    async fn send_to_dead_port_marks_failed() {
        let dir = tempfile::tempdir().unwrap();
        let service = test_service(dir.path(), None);
        // Bind then drop a listener so the port is (almost surely) closed.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let dead_port = listener.local_addr().unwrap().port();
        drop(listener);
        service.insert_peer_for_test("ghost", "ghost-host", IpAddr::V4(Ipv4Addr::LOCALHOST), dead_port);

        service.send("ghost", "text".into(), Some("anyone?".into()), None, None).unwrap();
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let messages = service.messages_json("ghost", None)["messages"].as_array().unwrap().clone();
            if messages[0]["status"] == "failed" {
                return;
            }
        }
        panic!("send to a dead port never flipped to failed");
    }

    #[tokio::test]
    async fn send_requires_known_peer_and_valid_payload() {
        let dir = tempfile::tempdir().unwrap();
        let service = test_service(dir.path(), None);
        let offline = service
            .send("nobody", "text".into(), Some("hi".into()), None, None)
            .unwrap_err();
        assert_eq!(offline.code, "PEER_OFFLINE");

        service.insert_peer_for_test("p", "h", IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
        let bad_kind = service.send("p", "audio".into(), None, None, None).unwrap_err();
        assert_eq!(bad_kind.code, "INVALID_PARAMS");
        let empty = service.send("p", "text".into(), Some(String::new()), None, None).unwrap_err();
        assert_eq!(empty.code, "INVALID_PARAMS");
        let no_image = service.send("p", "image".into(), None, None, None).unwrap_err();
        assert_eq!(no_image.code, "INVALID_PARAMS");
        let bad_mime =
            service.send("p", "image".into(), None, Some(b64(&tiny_png())), Some("image/gif".into())).unwrap_err();
        assert_eq!(bad_mime.code, "INVALID_PARAMS");
    }

    #[test]
    fn broadcast_targets_are_wellformed() {
        let targets = broadcast_targets(DISCOVERY_PORT);
        assert!(targets.contains(&SocketAddr::from(([255, 255, 255, 255], DISCOVERY_PORT))));
        for target in targets {
            assert_eq!(target.port(), DISCOVERY_PORT);
            assert!(!target.ip().is_loopback());
            assert!(!target.ip().is_unspecified());
        }
    }

    #[tokio::test]
    async fn persisted_store_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let service = test_service(dir.path(), None);
        {
            let mut state = service.state.lock().unwrap_or_else(|e| e.into_inner());
            state.conversations.insert(
                "peer-x".to_string(),
                vec![ChatMessage {
                    id: 42,
                    dir: Dir::Out,
                    kind: Kind::Text,
                    text: Some("persisted".into()),
                    image: None,
                    ts_ms: 1_700_000_000_000,
                    status: Some(Status::Sent),
                }],
            );
            service.persist_locked(&state);
        }
        let (conversations, max_id) = load_conversations(&dir.path().join("lan_chat.json"));
        assert_eq!(conversations["peer-x"][0].text.as_deref(), Some("persisted"));
        assert_eq!(conversations["peer-x"][0].status, Some(Status::Sent));
        assert_eq!(max_id, 42);
    }

    #[tokio::test]
    async fn conversation_cap_trims_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let service = test_service(dir.path(), None);
        for i in 0..(PERSIST_CAP + 30) {
            // Rotate source IPs so the per-IP rate limit never trips — this
            // test exercises the persistence cap, not the rate limiter.
            let ip = IpAddr::V4(Ipv4Addr::new(10, ((i / 200) % 250 + 1) as u8, (i % 200 + 1) as u8, 1));
            service
                .deliver_incoming(from("p"), "text".into(), Some(format!("m{i}")), None, None, None, ip)
                .await
                .unwrap();
        }
        let messages = service.messages_json("p", None)["messages"].as_array().unwrap().clone();
        assert_eq!(messages.len(), PERSIST_CAP);
        assert_eq!(messages[0]["text"], format!("m{}", 30));
        assert_eq!(messages.last().unwrap()["text"], format!("m{}", PERSIST_CAP + 29));
    }
}
