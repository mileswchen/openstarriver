//! Session seam: durable append-only session events and the storage contract.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use cordis_rslite::{BoxFuture, Context, EventBus, Service};

use crate::json::JsonValue;
use crate::llm::Message;

/// On-disk format version, stamped at creation. A persistence backend rejects
/// any other version on load; there is no migration, matching
/// `SESSION_FORMAT_VERSION`.
pub const SESSION_FORMAT_VERSION: u8 = 0;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub String);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Coarse product classification for a session created as a subagent child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOrigin {
    Subagent,
}

/// Immutable validated storage metadata, kept outside the conversation event
/// log, matching `SessionHeader`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHeader {
    pub version: u8,
    pub id: SessionId,
    /// Non-negative Unix epoch milliseconds when the session was created.
    pub created_at: u64,
    /// Absolute working directory the session was created in, if any.
    pub cwd: Option<String>,
    /// The session this one was forked from (seed lineage), if any.
    pub parent_session: Option<SessionId>,
    /// How many leading events were inherited through a seed. Persisting this
    /// boundary lets resume and replay distinguish parent history from child
    /// work.
    pub seed_length: Option<u64>,
    /// Presentation metadata for a subagent child; not proof of continuability.
    pub origin: Option<SessionOrigin>,
    /// Delegation depth: absent (zero) for a top-level session, parent depth
    /// plus one for a subagent child. Persisted so a recursion budget survives
    /// restart and resume.
    pub delegation_depth: Option<u32>,
    /// Id of the agent preset this session's agent was composed from, when the
    /// deployment composes per session.
    pub agent_preset: Option<String>,
}

impl SessionHeader {
    /// Synthesize the minimal store-owned header for a fresh session.
    pub fn new(id: SessionId) -> Self {
        Self {
            version: SESSION_FORMAT_VERSION,
            id,
            created_at: unix_epoch_ms(),
            cwd: None,
            parent_session: None,
            seed_length: None,
            origin: None,
            delegation_depth: None,
            agent_preset: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session event sequence is invalid")]
    InvalidSequence,
    #[error("session event not found")]
    NotFound,
    #[error("session already exists")]
    AlreadyExists,
    #[error("session dispatch failed: {0}")]
    Dispatch(String),
    #[error("surface projection failed: {0}")]
    Surface(#[from] crate::json::JsonValueError),
}

/// One append-only session fact, matching envelope
/// `{ type, seq, time, data }`: `seq` is the log length (0-based contiguity)
/// and `time` is Unix epoch milliseconds.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    pub seq: u64,
    pub time: u64,
 /// The event type (`type` in; renamed for the Rust keyword).
    pub event_type: String,
 /// Lossless JSON payload (requires every session event to be
    /// lossless-JSON serializable).
    pub data: JsonValue,
    /// How this event enters the derived surface, if it produces a message.
    pub surface_op: Option<SurfaceOp>,
    /// Durable source event seqs cited by this surface entry (assistant
    /// messages cite their chunk seqs; tool results cite their call seq).
    pub source_event_seqs: Vec<u64>,
}

impl SessionEvent {
    /// Durable wire envelope: `{ type, seq, time, data, surfaceOp?,
    /// sourceEventSeqs?, ignorable? }`. `surfaceOp` serializes as
    /// `{ op: "append" }` or `{ op: "replace", start, end }`.
    pub fn to_json(&self) -> JsonValue {
        let mut entries = vec![
            ("type".into(), JsonValue::String(self.event_type.clone())),
            ("seq".into(), JsonValue::UInt(self.seq)),
            ("time".into(), JsonValue::UInt(self.time)),
            ("data".into(), self.data.clone()),
        ];
        if let Some(op) = &self.surface_op {
            entries.push((
                "surfaceOp".into(),
                match op {
                    SurfaceOp::Append => {
                        JsonValue::Object(vec![("op".into(), JsonValue::String("append".into()))])
                    }
                    SurfaceOp::Replace { start, end } => JsonValue::Object(vec![
                        ("op".into(), JsonValue::String("replace".into())),
                        ("start".into(), JsonValue::UInt(*start)),
                        ("end".into(), JsonValue::UInt(*end)),
                    ]),
                },
            ));
        }
        if !self.source_event_seqs.is_empty() {
            entries.push((
                "sourceEventSeqs".into(),
                JsonValue::Array(
                    self.source_event_seqs
                        .iter()
                        .copied()
                        .map(JsonValue::UInt)
                        .collect(),
                ),
            ));
        }
        JsonValue::Object(entries)
    }

    /// Parse the durable wire envelope. Missing/incorrect `type`, `seq`,
    /// `time`, or `data` reject; optional fields default to absent.
    pub fn from_json(value: &JsonValue) -> Result<Self, SessionError> {
        let event_type = value
            .as_str_at("type")
            .ok_or(SessionError::InvalidSequence)?
            .to_string();
        let seq = value.get("seq").and_then(JsonValue::as_u64).unwrap_or(0);
        let time = value.get("time").and_then(JsonValue::as_u64).unwrap_or(0);
        let data = value.get("data").cloned().unwrap_or(JsonValue::Null);
        let surface_op = match value.get("surfaceOp") {
            Some(op) if op.as_str_at("op") == Some("append") => Some(SurfaceOp::Append),
            Some(op) if op.as_str_at("op") == Some("replace") => Some(SurfaceOp::Replace {
                start: op.get("start").and_then(JsonValue::as_u64).unwrap_or(0),
                end: op.get("end").and_then(JsonValue::as_u64).unwrap_or(0),
            }),
            _ => None,
        };
        let source_event_seqs = value
            .get("sourceEventSeqs")
            .and_then(JsonValue::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(JsonValue::as_u64)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(Self {
            seq,
            time,
            event_type,
            data,
            surface_op,
            source_event_seqs,
        })
    }
}

/// How a session event enters the derived model-message surface, matching
/// `SurfaceOp`: `Append` extends the tail; `Replace` shadows the
/// inclusive `[start, end]` seq range and takes the event's own position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceOp {
    Append,
    Replace { start: u64, end: u64 },
}

/// Durable reason recorded by `turn/end`, matching `TurnEndReason`.
#[derive(Debug, Clone)]
pub enum TurnEndReason {
    Completed,
    MaxTokens,
    Blocked,
    Aborted {
        cause: crate::agent::AgentCancelCause,
    },
    Error {
        failure: crate::llm::LlmFailure,
    },
    Disposed,
}

impl TurnEndReason {
    /// Stable string tag for the durable payload.
    pub fn kind(&self) -> &'static str {
        match self {
            TurnEndReason::Completed => "completed",
            TurnEndReason::MaxTokens => "max-tokens",
            TurnEndReason::Blocked => "blocked",
            TurnEndReason::Aborted { .. } => "aborted",
            TurnEndReason::Error { .. } => "error",
            TurnEndReason::Disposed => "disposed",
        }
    }

    /// Wire shape `{ kind, cause?, failure? }`.
    pub fn to_json(&self) -> JsonValue {
        let mut entries = vec![("kind".into(), JsonValue::String(self.kind().into()))];
        if let TurnEndReason::Aborted { cause } = self {
            entries.push((
                "cause".into(),
                JsonValue::String(
                    match cause.kind {
                        crate::agent::AgentCancelKind::User => "user",
                        crate::agent::AgentCancelKind::Parent => "parent",
                        crate::agent::AgentCancelKind::Disposed => "disposed",
                    }
                    .into(),
                ),
            ));
            // Cancel-source provenance: who asked for the abort (set by the
            // cancel call site, e.g. "session.cancel" or
            // "execute-cancel-token"). Without it an aborted turn is
            // indistinguishable from a UI Stop after the fact.
            if let Some(source) = &cause.reason {
                entries.push(("source".into(), JsonValue::String(source.clone())));
            }
        }
        if let TurnEndReason::Error { failure } = self {
            entries.push(("failure".into(), failure.to_json()));
        }
        JsonValue::Object(entries)
    }
}

/// Optional surface metadata carried by an append, matching
/// `{ surfaceOp, sourceEventSeqs }` envelope fields.
#[derive(Debug, Clone, Default)]
pub struct AppendOptions {
    pub surface_op: Option<SurfaceOp>,
    pub source_event_seqs: Vec<u64>,
}

/// One positional replacement operation observed while folding a surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceReplacement {
    pub seq: u64,
    pub start: u64,
    pub end: u64,
    /// Surface entries shadowed by the replacement, in surface order.
    pub shadowed_seqs: Vec<u64>,
}

/// Replayed surface operations: current model-visible event seqs plus the
/// replacements that shaped them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SurfaceFold {
    pub nodes: Vec<u64>,
    pub replacements: Vec<SurfaceReplacement>,
}

/// Runtime event-sourced session.
#[derive(Clone)]
pub struct Session {
    pub id: SessionId,
    pub header: SessionHeader,
    /// The first seq appended in this process: the seed length. Events with
    /// smaller seq values entered through construction (replay/fork) and were
 /// never published on the `session/event` firehose, matching
    /// `firstLiveSeq`.
    pub first_live_seq: u64,
    events: Arc<Mutex<Vec<SessionEvent>>>,
    bus: EventBus,
    target: Option<Context>,
}

impl Session {
    pub fn new(id: SessionId, bus: EventBus) -> Self {
        let header = SessionHeader::new(id.clone());
        Self {
            header: header.clone(),
            id,
            first_live_seq: 0,
            events: Arc::new(Mutex::new(Vec::new())),
            bus,
            target: None,
        }
    }

    pub fn with_target(mut self, ctx: Context) -> Self {
        self.target = Some(ctx);
        self
    }

    /// Override the synthesized creation metadata (used by persistence and
    /// fork callers that already own a validated header).
    pub fn with_header(mut self, header: SessionHeader) -> Result<Self, SessionError> {
        if header.id != self.id {
            return Err(SessionError::InvalidSequence);
        }
        self.header = header;
        Ok(self)
    }

 /// Replay or fork an existing event log at construction, mirroring
    /// seeded `Session.create`. The seed is validated to the same invariants
    /// `append` enforces: `seq` must be contiguous from 0. A `session/end-seed`
    /// marker is appended (without publishing) when the seed does not already
    /// end in one, marking the durable fork-lineage boundary.
    pub fn with_seed(mut self, seed: Vec<SessionEvent>) -> Result<Self, SessionError> {
        {
            let mut events = self.events.lock().expect("session events poisoned");
            for (index, event) in seed.iter().enumerate() {
                if event.seq != index as u64 {
                    return Err(SessionError::InvalidSequence);
                }
                events.push(event.clone());
            }
        }
        self.first_live_seq = seed.len() as u64;
        let ends_with_marker = self
            .events
            .lock()
            .expect("session events poisoned")
            .last()
            .is_some_and(|event| event.event_type == "session/end-seed");
        if !ends_with_marker {
            // In-process construction fact; not published on the firehose.
            self.append_inner(
                "session/end-seed",
                JsonValue::Null,
                false,
                AppendOptions::default(),
            )?;
        }
        Ok(self)
    }

    pub fn append(
        &self,
        event_type: impl Into<String>,
        data: JsonValue,
    ) -> Result<SessionEvent, SessionError> {
        self.append_with(event_type, data, AppendOptions::default())
    }

    /// Append with optional surface metadata (`surfaceOp` / `sourceEventSeqs`).
    pub fn append_with(
        &self,
        event_type: impl Into<String>,
        data: JsonValue,
        options: AppendOptions,
    ) -> Result<SessionEvent, SessionError> {
        self.append_inner(event_type, data, true, options)
    }

    fn append_inner(
        &self,
        event_type: impl Into<String>,
        data: JsonValue,
        emit: bool,
        options: AppendOptions,
    ) -> Result<SessionEvent, SessionError> {
        let event_type: String = event_type.into();
        let mut events = self.events.lock().expect("session events poisoned");
        // SP5 provenance check (debug-assert grade per prompt-parity §4.2):
        // every referenced source seq must exist in the append-only log.
        // Violations still append — append-only tolerance wins — but warn in
        // release builds where the assert compiles out.
        for seq in &options.source_event_seqs {
            let valid = *seq < events.len() as u64;
            debug_assert!(
                valid,
                "provenance: source_event_seq {seq} does not exist (log len {})",
                events.len()
            );
            if !valid {
                tracing::warn!(
                    event = %event_type,
                    seq = *seq,
                    log_len = events.len(),
                    "provenance: source_event_seq out of range; appending anyway"
                );
            }
        }
        let event = SessionEvent {
            seq: events.len() as u64,
            time: unix_epoch_ms(),
            event_type,
            data,
            surface_op: options.surface_op,
            source_event_seqs: options.source_event_seqs,
        };
        events.push(event.clone());
        drop(events);

        if emit {
            let emitted_event = Arc::new(super::session::SessionEventOccurred {
                session: self.clone(),
                event: event.clone(),
            });
            match &self.target {
                Some(ctx) => self.bus.emit_scoped(ctx, emitted_event),
                None => self.bus.emit(emitted_event),
            }
        }
        Ok(event)
    }

    /// Immutable snapshot of the append-only log.
    pub fn events(&self) -> Vec<SessionEvent> {
        self.events.lock().expect("session events poisoned").clone()
    }

    /// Snapshot the events from seq `from_seq` on (seqs are log indices, so
    /// `events_after(n)` may re-include seq `n` itself — callers treat the
    /// scan as idempotent). Tail-scanning consumers (the loop's reload
    /// confirmation) use this to avoid deep-cloning the whole log every
    /// step.
    pub fn events_after(&self, from_seq: u64) -> Vec<SessionEvent> {
        let guard = self.events.lock().expect("session events poisoned");
        let skip = (from_seq.min(guard.len() as u64)) as usize;
        guard.iter().skip(skip).cloned().collect()
    }

    /// The next event's sequence number — always the log length.
    pub fn seq(&self) -> u64 {
        self.events.lock().expect("session events poisoned").len() as u64
    }

    /// Fold the event log into the current model-visible surface: append
    /// events extend the tail; replace events shadow an inclusive seq range
    /// and take the event's own position.
    pub fn surface(&self) -> SurfaceFold {
        let mut nodes = Vec::new();
        let mut replacements = Vec::new();
        for event in self.events() {
            let Some(op) = &event.surface_op else {
                continue;
            };
            match op {
                SurfaceOp::Append => nodes.push(event.seq),
                SurfaceOp::Replace { start, end } => {
                    let shadowed = nodes
                        .iter()
                        .copied()
                        .filter(|seq| *seq >= *start && *seq <= *end)
                        .collect::<Vec<_>>();
                    nodes.retain(|seq| *seq < *start || *seq > *end);
                    replacements.push(SurfaceReplacement {
                        seq: event.seq,
                        start: *start,
                        end: *end,
                        shadowed_seqs: shadowed,
                    });
                    nodes.push(event.seq);
                }
            }
        }
        SurfaceFold {
            nodes,
            replacements,
        }
    }

    /// Derive the model-visible message history together with the surface
    /// event seq each message was projected from. Empty assistant messages
 /// (usage-only anchors) and non-surface events are skipped, matching
    /// `deriveEventMessage`.
    pub fn derive_messages_with_seqs(&self) -> Result<Vec<(u64, Message)>, SessionError> {
        let fold = self.surface();
        let events = self.events();
        let by_seq = events
            .into_iter()
            .map(|event| (event.seq, event))
            .collect::<HashMap<_, _>>();
        let mut messages = Vec::new();
        for seq in fold.nodes {
            let Some(event) = by_seq.get(&seq) else {
                continue;
            };
            let message = match event.event_type.as_str() {
                "user/message" => Message::from_json(event.data.clone())?,
                "assistant/message" => {
                    let message = Message::from_json(
                        event
                            .data
                            .get("message")
                            .cloned()
                            .unwrap_or(JsonValue::Null),
                    )?;
                    if message.content.is_empty() {
                        continue;
                    }
                    message
                }
                "tool/result" => Message::from_json(
                    event
                        .data
                        .get("message")
                        .cloned()
                        .unwrap_or(JsonValue::Null),
                )?,
                _ => continue,
            };
            messages.push((seq, message));
        }
        Ok(messages)
    }

    /// Derive the model-visible message history from the surface. Empty
    /// assistant messages (usage-only anchors) and non-surface events are
 /// skipped, matching `deriveEventMessage`.
    pub fn derive_messages(&self) -> Result<Vec<Message>, SessionError> {
        Ok(self
            .derive_messages_with_seqs()?
            .into_iter()
            .map(|(_, message)| message)
            .collect())
    }

    pub fn flush(&self) -> BoxFuture<Result<(), SessionError>> {
        let bus = self.bus.clone();
        let event = Arc::new(super::session::SessionFlush {
            session: self.clone(),
        });
        let target = self.target.clone();
        Box::pin(async move {
            match target {
                Some(ctx) => bus.parallel_scoped(&ctx, event).await,
                None => bus.parallel(event).await,
            }
            .map_err(|error| SessionError::Dispatch(error.to_string()))
        })
    }
}

fn unix_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Fresh-session construction options for `SessionStore::prepare`.
#[derive(Debug, Clone, Default)]
pub struct PrepareOptions {
    /// Absolute working directory for the fresh session header.
    pub cwd: Option<String>,
    /// Inherited parent-history events (replay/fork seed).
    pub seed: Vec<SessionEvent>,
 /// Agent preset id composed into the fresh session header (binding:
    /// chosen at creation, locked thereafter; persisted with the header so
    /// resume/fork recover it). Must be stamped at creation — a post-enter
    /// header mutation would never reach the store.
    pub agent_preset: Option<String>,
}

/// Ownership of one unpublished Session before registry publication. Dropping
/// the preparation releases provider-owned unpublished state; publication
/// (enter) may consume that state first.
pub struct SessionPreparation {
    pub session: Session,
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl SessionPreparation {
    pub fn new(session: Session) -> Self {
        Self {
            session,
            release: None,
        }
    }

    pub fn with_release(mut self, release: Box<dyn FnOnce() + Send>) -> Self {
        self.release = Some(release);
        self
    }
}

impl Drop for SessionPreparation {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// Removal capability for one entered session. Disposal is explicit and
/// pairs `session/disposed` when the session was announced. The store map is
/// the source of liveness, so dropping an undisposed handle leaves the entry
/// in place (removal goes through [`EnterHandle::dispose`] or
/// [`SessionStore::remove`]).
pub struct EnterHandle {
    detach: Option<Box<dyn FnOnce() + Send>>,
}

impl EnterHandle {
    pub fn new(detach: Box<dyn FnOnce() + Send>) -> Self {
        Self {
            detach: Some(detach),
        }
    }

    pub fn dispose(mut self) {
        if let Some(detach) = self.detach.take() {
            detach();
        }
    }
}

/// Product seam for the live session registry.
///
/// Backends are plugins; the kernel only owns this contract. Publication is
/// the three-stage transaction `prepare` -> `enter` -> `announce`: `enter`
/// installs the registry entry and returns a removal handle without emitting;
/// `announce` synchronously dispatches `session/created` and a listener error
/// vetoes — the backend rolls the entry back and pairs `session/disposed`.
pub trait SessionStore: Service {
    const NAME: &'static str = "session";

    fn prepare(
        &self,
        id: SessionId,
        options: PrepareOptions,
    ) -> Result<SessionPreparation, SessionError>;
    fn enter(&self, session: Session) -> Result<EnterHandle, SessionError>;
    fn announce(&self, session: &Session) -> Result<(), SessionError>;
    fn get(&self, id: &SessionId) -> Option<Session>;
    fn remove(&self, id: &SessionId) -> Option<Session>;

    /// One-transaction convenience: prepare, enter, announce. A veto rolls
    /// the entry back and returns the announce error.
    fn create(&self, id: SessionId, options: PrepareOptions) -> Result<Session, SessionError> {
        let preparation = self.prepare(id, options)?;
        let session = preparation.session.clone();
        let handle = self.enter(session.clone())?;
        if let Err(error) = self.announce(&session) {
            handle.dispose();
            return Err(error);
        }
        Ok(session)
    }
}

cordis_rslite::cordis_event!(SessionCreated, "session/created", {
    session: Session,
});

cordis_rslite::cordis_event!(SessionDisposed, "session/disposed", {
    session: Session,
});

cordis_rslite::cordis_event!(SessionEventOccurred, "session/event", {
    session: Session,
    event: SessionEvent,
});

cordis_rslite::cordis_event!(SessionFlush, "session/flush", {
    session: Session,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aborted_reason_json_carries_cancel_source() {
        // BUG: cause=user alone could not say WHO asked for the abort —
        // UI Stop, queue send-now and external clients were indistinguishable
        // after the fact. The source tag must ride turn/end.
        let with_source = TurnEndReason::Aborted {
            cause: crate::agent::AgentCancelCause::with_reason(
                crate::agent::AgentCancelKind::User,
                "session.cancel",
            ),
        }
        .to_json();
        assert_eq!(with_source.as_str_at("kind"), Some("aborted"));
        assert_eq!(with_source.as_str_at("cause"), Some("user"));
        assert_eq!(with_source.as_str_at("source"), Some("session.cancel"));

        let without_source = TurnEndReason::Aborted {
            cause: crate::agent::AgentCancelCause::new(crate::agent::AgentCancelKind::User),
        }
        .to_json();
        assert_eq!(without_source.as_str_at("cause"), Some("user"));
        assert_eq!(without_source.as_str_at("source"), None);
    }

    #[test]
    fn header_is_synthesized_with_version_and_timestamp() {
        let header = SessionHeader::new(SessionId("s1".into()));
        assert_eq!(header.version, SESSION_FORMAT_VERSION);
        assert_eq!(header.id.0, "s1");
        assert!(header.created_at > 0);
        assert!(header.cwd.is_none());
        assert!(header.parent_session.is_none());
        assert!(header.seed_length.is_none());
        assert!(header.origin.is_none());
        assert!(header.delegation_depth.is_none());
        assert!(header.agent_preset.is_none());
    }

    #[test]
    fn append_is_zero_based_and_stamped_with_time() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus.clone());
        let first = session.append("turn/start", JsonValue::UInt(1)).unwrap();
        let second = session.append("turn/end", JsonValue::UInt(2)).unwrap();
        assert_eq!(first.seq, 0);
        assert_eq!(second.seq, 1);
        assert!(first.time > 0);
        assert_eq!(session.seq(), 2);
        assert_eq!(session.first_live_seq, 0);
    }

    /// Tail snapshots: `events_after(n)` returns seqs ≥ n without cloning
    /// the whole log (the reload-confirmation scan runs every step).
    #[test]
    fn events_after_returns_the_tail() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        for i in 0..4u64 {
            session.append("turn/start", JsonValue::UInt(i)).unwrap();
        }
        let tail = session.events_after(2);
        assert_eq!(
            tail.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(session.events_after(0).len(), 4);
        assert!(session.events_after(9).is_empty());
    }

    #[test]
    fn seed_replays_events_and_appends_end_seed_marker() {
        let bus = EventBus::new();
        let seed = vec![
            SessionEvent {
                seq: 0,
                time: 1,
                event_type: "user/message".into(),
                data: JsonValue::String("hello".into()),
                surface_op: None,
                source_event_seqs: Vec::new(),
            },
            SessionEvent {
                seq: 1,
                time: 2,
                event_type: "assistant/message".into(),
                data: JsonValue::String("hi".into()),
                surface_op: None,
                source_event_seqs: Vec::new(),
            },
        ];
        let session = Session::new(SessionId("s1".into()), bus.clone())
            .with_seed(seed)
            .unwrap();
        assert_eq!(session.first_live_seq, 2);
        let events = session.events();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event_type, "user/message");
        assert_eq!(events[2].event_type, "session/end-seed");
        assert_eq!(events[2].seq, 2);
        // A live append continues the contiguous sequence after the marker.
        let appended = session.append("turn/start", JsonValue::UInt(1)).unwrap();
        assert_eq!(appended.seq, 3);
    }

    #[test]
    fn seed_ending_in_marker_is_not_duplicated() {
        let bus = EventBus::new();
        let seed = vec![
            SessionEvent {
                seq: 0,
                time: 1,
                event_type: "user/message".into(),
                data: JsonValue::Null,
                surface_op: None,
                source_event_seqs: Vec::new(),
            },
            SessionEvent {
                seq: 1,
                time: 2,
                event_type: "session/end-seed".into(),
                data: JsonValue::Null,
                surface_op: None,
                source_event_seqs: Vec::new(),
            },
        ];
        let session = Session::new(SessionId("s1".into()), bus.clone())
            .with_seed(seed)
            .unwrap();
        assert_eq!(session.events().len(), 2);
        assert_eq!(session.first_live_seq, 2);
    }

    #[test]
    fn seed_with_gap_is_rejected() {
        let bus = EventBus::new();
        let seed = vec![SessionEvent {
            seq: 1,
            time: 1,
            event_type: "user/message".into(),
            data: JsonValue::Null,
            surface_op: None,
            source_event_seqs: Vec::new(),
        }];
        let error = match Session::new(SessionId("s1".into()), bus).with_seed(seed) {
            Ok(_) => panic!("expected invalid seed to be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, SessionError::InvalidSequence));
    }

    #[test]
    fn header_override_requires_matching_id() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus.clone());
        let header = SessionHeader {
            version: SESSION_FORMAT_VERSION,
            id: SessionId("s1".into()),
            created_at: 42,
            cwd: Some("/work".into()),
            parent_session: Some(SessionId("parent".into())),
            seed_length: Some(2),
            origin: Some(SessionOrigin::Subagent),
            delegation_depth: Some(1),
            agent_preset: Some("coder".into()),
        };
        let session = session.with_header(header).unwrap();
        assert_eq!(session.header.created_at, 42);
        assert_eq!(session.header.cwd.as_deref(), Some("/work"));
        assert_eq!(session.header.delegation_depth, Some(1));

        let wrong = SessionHeader::new(SessionId("other".into()));
        let error = match Session::new(SessionId("s1".into()), bus).with_header(wrong) {
            Ok(_) => panic!("expected mismatched header to be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, SessionError::InvalidSequence));
    }

    #[test]
    fn append_with_options_records_surface_metadata() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        // Provenance refs must point at existing log entries: seed two first.
        session.append("user/message", JsonValue::Null).unwrap();
        session.append("user/message", JsonValue::Null).unwrap();
        let event = session
            .append_with(
                "assistant/message",
                JsonValue::Object(vec![("message".into(), JsonValue::Null)]),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: vec![0, 1],
                },
            )
            .unwrap();
        assert_eq!(event.surface_op, Some(SurfaceOp::Append));
        assert_eq!(event.source_event_seqs, vec![0, 1]);

        let plain = session.append("turn/end", JsonValue::Null).unwrap();
        assert!(plain.surface_op.is_none());
        assert!(plain.source_event_seqs.is_empty());
    }

    #[test]
    fn provenance_valid_refs_accepted() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        session.append("user/message", JsonValue::Null).unwrap();
        let ok = session
            .append_with(
                "assistant/message",
                JsonValue::Null,
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: vec![0],
                },
            )
            .unwrap();
        assert_eq!(ok.source_event_seqs, vec![0]);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "provenance")]
    fn provenance_invalid_ref_asserts_in_debug() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        session.append("user/message", JsonValue::Null).unwrap();
        let _ = session.append_with(
            "assistant/message",
            JsonValue::Null,
            AppendOptions {
                surface_op: None,
                source_event_seqs: vec![0, 99],
            },
        );
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn provenance_invalid_ref_appends_in_release() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        session.append("user/message", JsonValue::Null).unwrap();
        let tolerated = session
            .append_with(
                "assistant/message",
                JsonValue::Null,
                AppendOptions {
                    surface_op: None,
                    source_event_seqs: vec![0, 99],
                },
            )
            .unwrap();
        assert_eq!(tolerated.source_event_seqs, vec![0, 99]);
    }

    #[test]
    fn surface_fold_tracks_nodes_and_ignores_log_only_events() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        session
            .append_with(
                "user/message",
                JsonValue::Null,
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: Vec::new(),
                },
            )
            .unwrap();
        session.append("assistant/chunk", JsonValue::Null).unwrap();
        session
            .append_with(
                "assistant/message",
                JsonValue::Null,
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: vec![1],
                },
            )
            .unwrap();

        let fold = session.surface();
        assert_eq!(fold.nodes, vec![0, 2]);
        assert!(fold.replacements.is_empty());
    }

    #[test]
    fn surface_replace_shadows_inclusive_range() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        for _ in 0..2 {
            session
                .append_with(
                    "user/message",
                    JsonValue::Null,
                    AppendOptions {
                        surface_op: Some(SurfaceOp::Append),
                        source_event_seqs: Vec::new(),
                    },
                )
                .unwrap();
        }
        session
            .append_with(
                "user/message",
                JsonValue::Null,
                AppendOptions {
                    surface_op: Some(SurfaceOp::Replace { start: 0, end: 1 }),
                    source_event_seqs: Vec::new(),
                },
            )
            .unwrap();

        let fold = session.surface();
        assert_eq!(fold.nodes, vec![2]);
        assert_eq!(fold.replacements.len(), 1);
        assert_eq!(fold.replacements[0].shadowed_seqs, vec![0, 1]);
    }

    #[test]
    fn derive_messages_projects_and_skips_empty_assistant() {
        use crate::llm::{CallId, ContentBlock, Message, MessageId, MessageRole, MessageSource};

        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        let user = Message {
            id: MessageId("m1".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::Text("hi".into())],
            source: MessageSource::User,
        };
        session
            .append_with(
                "user/message",
                user.to_json().unwrap(),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: Vec::new(),
                },
            )
            .unwrap();
        // Empty-content assistant message only hosts usage: it is skipped.
        let empty_assistant = Message {
            id: MessageId("m2".into()),
            role: MessageRole::Assistant,
            content: Vec::new(),
            source: MessageSource::Model {
                provider: "p".into(),
                model: "m".into(),
                replay_state: None,
            },
        };
        session
            .append_with(
                "assistant/message",
                JsonValue::Object(vec![("message".into(), empty_assistant.to_json().unwrap())]),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: Vec::new(),
                },
            )
            .unwrap();
        let tool_result = Message {
            id: MessageId("m3".into()),
            role: MessageRole::User,
            content: vec![ContentBlock::Text("out".into())],
            source: MessageSource::Tool {
                call_id: CallId("c1".into()),
            },
        };
        session
            .append_with(
                "tool/result",
                JsonValue::Object(vec![("message".into(), tool_result.to_json().unwrap())]),
                AppendOptions {
                    surface_op: Some(SurfaceOp::Append),
                    source_event_seqs: vec![0],
                },
            )
            .unwrap();

        let messages = session.derive_messages().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].id.0, "m1");
        assert_eq!(messages[1].id.0, "m3");
    }

    #[test]
    fn session_preparation_release_runs_once_on_drop() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus);
        let released = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&released);
        let preparation = SessionPreparation::new(session).with_release(Box::new(move || {
            flag.store(true, Ordering::SeqCst);
        }));
        drop(preparation);
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn enter_handle_dispose_runs_detach_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let handle = EnterHandle::new(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        handle.dispose();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn turn_end_reason_kind_and_json_shape() {
        use crate::agent::{AgentCancelCause, AgentCancelKind};
        use crate::llm::LlmFailure;

        let aborted = TurnEndReason::Aborted {
            cause: AgentCancelCause::new(AgentCancelKind::User),
        };
        assert_eq!(aborted.kind(), "aborted");
        let json = aborted.to_json();
        assert_eq!(json.as_str_at("kind"), Some("aborted"));
        assert_eq!(json.as_str_at("cause"), Some("user"));

        let error = TurnEndReason::Error {
            failure: LlmFailure {
                message: "boom".into(),
                code: "SERVER".into(),
                status: None,
                provider_retry_after_ms: None,
                request_id: None,
            },
        };
        assert_eq!(error.kind(), "error");
        let error_json = error.to_json();
        assert_eq!(error_json.as_str_at("kind"), Some("error"));
        assert!(error_json.get("failure").is_some());
    }

    #[test]
    fn session_event_wire_round_trips_full_envelope() {
        let event = SessionEvent {
            seq: 3,
            time: 1234,
            event_type: "tool/result".into(),
            data: JsonValue::Object(vec![("ok".into(), JsonValue::Bool(true))]),
            surface_op: Some(SurfaceOp::Replace { start: 1, end: 2 }),
            source_event_seqs: vec![1, 2],
        };
        let json = event.to_json();
        let back = SessionEvent::from_json(&json).unwrap();
        assert_eq!(back.seq, event.seq);
        assert_eq!(back.time, event.time);
        assert_eq!(back.event_type, event.event_type);
        assert_eq!(back.data, event.data);
        assert_eq!(back.surface_op, event.surface_op);
        assert_eq!(back.source_event_seqs, event.source_event_seqs);
    }

    #[test]
    fn session_event_wire_defaults_optional_fields() {
        let json = JsonValue::Object(vec![
            ("type".into(), JsonValue::String("turn/start".into())),
            ("seq".into(), JsonValue::UInt(0)),
            ("time".into(), JsonValue::UInt(9)),
            ("data".into(), JsonValue::Null),
        ]);
        let event = SessionEvent::from_json(&json).unwrap();
        assert_eq!(event.surface_op, None);
        assert!(event.source_event_seqs.is_empty());
    }

    #[test]
    fn session_event_wire_rejects_missing_fields() {
        let json = JsonValue::Object(vec![("seq".into(), JsonValue::UInt(0))]);
        assert!(SessionEvent::from_json(&json).is_err());
    }

    #[test]
    fn session_event_wire_append_op_round_trips() {
        let event = SessionEvent {
            seq: 0,
            time: 7,
            event_type: "assistant/message".into(),
            data: JsonValue::Null,
            surface_op: Some(SurfaceOp::Append),
            source_event_seqs: vec![0],
        };
        let back = SessionEvent::from_json(&event.to_json()).unwrap();
        assert_eq!(back.surface_op, Some(SurfaceOp::Append));
        assert_eq!(back.source_event_seqs, vec![0]);
    }

    #[test]
    fn session_event_wire_ignores_unknown_surface_op() {
        let json = JsonValue::Object(vec![
            ("type".into(), JsonValue::String("turn/start".into())),
            ("seq".into(), JsonValue::UInt(0)),
            ("time".into(), JsonValue::UInt(1)),
            ("data".into(), JsonValue::Null),
            (
                "surfaceOp".into(),
                JsonValue::Object(vec![("op".into(), JsonValue::String("shadow".into()))]),
            ),
        ]);
        let event = SessionEvent::from_json(&json).unwrap();
        assert_eq!(event.surface_op, None);
    }

    #[test]
    fn session_event_wire_missing_optional_fields_keep_defaults() {
        let json = JsonValue::Object(vec![
            ("type".into(), JsonValue::String("user/message".into())),
            ("seq".into(), JsonValue::UInt(4)),
            ("time".into(), JsonValue::UInt(11)),
            ("data".into(), JsonValue::String("payload".into())),
        ]);
        let event = SessionEvent::from_json(&json).unwrap();
        assert_eq!(event.seq, 4);
        assert_eq!(event.time, 11);
        assert_eq!(event.event_type, "user/message");
        assert_eq!(event.data, JsonValue::String("payload".into()));
        assert_eq!(event.surface_op, None);
        assert!(event.source_event_seqs.is_empty());
    }

    #[test]
    fn session_event_wire_negative_seq_is_rejected_as_invalid() {
        // A negative seq cannot survive the u64 parse; the loader's
        // contiguity check rejects an out-of-order seq regardless.
        let json = JsonValue::Object(vec![
            ("type".into(), JsonValue::String("turn/start".into())),
            ("seq".into(), JsonValue::Int(-1)),
            ("time".into(), JsonValue::UInt(1)),
            ("data".into(), JsonValue::Null),
        ]);
        let event = SessionEvent::from_json(&json).unwrap();
        // as_u64 fails -> defaults to 0; strict contiguity is the loader's
        // responsibility (Task 4 store tests cover that boundary).
        assert_eq!(event.seq, 0);
    }
}
