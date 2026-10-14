//! Write-behind coordinator: one writer task per process, batched durable
//! writes with a fixed merge window, flush barriers, and failure retention.
//! Cross-process concurrency is handled by immediate SQLite transactions.

use std::collections::HashMap;
use std::sync::Arc;

use starriver_kernel::persistence::{PersistenceError, SessionInspection};
use starriver_kernel::session::{
    Session, SessionEvent, SessionEventOccurred, SessionHeader, SessionId,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::prepare::{PersistedPreparation, PrepareError, PrepareMsg};
use crate::store;
use cordis_rslite::EventBus;

/// Default maximum intentional batching wait (ms).
pub const DEFAULT_WRITE_BATCH_MAX_DELAY_MS: u64 = 200;
/// Bounded channel capacity; producers await when full (backpressure).
pub const DEFAULT_CHANNEL_CAPACITY: usize = 1024;
/// Bounded ready-source cache capacity (default is 5).
pub const DEFAULT_PREPARED_CACHE_SIZE: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error("coordinator is shut down")]
    Shutdown,
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

/// Ready-source cache entry: a constructed session plus the durable revision
/// it was built from. Reused only when the revision is still current.
struct CacheEntry {
    session: Session,
    revision: String,
}

pub struct WriteCoordinator {
    tx: mpsc::Sender<WriterMsg>,
    live_tx: mpsc::UnboundedSender<WriterMsg>,
    shared: Arc<std::sync::Mutex<rusqlite::Connection>>,
    store_id: String,
}

enum WriterMsg {
    Append {
        id: SessionId,
        header: SessionHeader,
        events: Vec<SessionEvent>,
        ack: oneshot::Sender<Result<(), CoordinatorError>>,
    },
    Flush {
        ack: oneshot::Sender<Result<(), CoordinatorError>>,
    },
    /// Fire-and-forget event from a live session's firehose; batched by the
    /// merge window and persisted without a caller ack.
    LiveAppend {
        id: SessionId,
        header: SessionHeader,
        events: Vec<SessionEvent>,
    },
    Prepare {
        msg: PrepareMsg,
    },
}

/// One session's grouped durable batch and its callers' acks.
type GroupedBatch = (
    SessionId,
    SessionHeader,
    Vec<SessionEvent>,
    Vec<oneshot::Sender<Result<(), CoordinatorError>>>,
);

impl WriteCoordinator {
    pub fn spawn(
        db: rusqlite::Connection,
        store_id: String,
        bus: EventBus,
        window_ms: u64,
        capacity: usize,
    ) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        let (live_tx, live_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(std::sync::Mutex::new(db));
        let cache = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let loop_store_id = store_id.clone();
        let loop_shared = Arc::clone(&shared);
        let loop_cache = Arc::clone(&cache);
        let loop_bus = bus.clone();
        tokio::spawn(async move {
            writer_loop(
                loop_shared,
                loop_cache,
                &loop_store_id,
                loop_bus,
                window_ms,
                rx,
                live_rx,
            )
            .await;
        });
        // Live sessions append through the `session/event` firehose; the
        // coordinator forwards each occurrence to the writer's live queue
        // (write-behind, no caller ack). Seed events and the `session/end-seed`
        // marker never emit, so they are not double-persisted.
        let forward = live_tx.clone();
        let handle = bus.on_sync::<SessionEventOccurred, _>(move |occurred| {
            let event = occurred.event.clone();
            let id = occurred.session.id.clone();
            let header = occurred.session.header.clone();
            let _ = forward.send(WriterMsg::LiveAppend {
                id,
                header,
                events: vec![event],
            });
            Ok(cordis_rslite::Value::new(()))
        });
        // The listener lives as long as the bus (and therefore this
        // coordinator): forget the handle instead of warning on drop.
        handle.forget();
        Self {
            tx,
            live_tx,
            shared,
            store_id,
        }
    }

    pub async fn append(
        &self,
        id: SessionId,
        header: SessionHeader,
        events: Vec<SessionEvent>,
    ) -> Result<(), CoordinatorError> {
        let (ack, rx) = oneshot::channel();
        self.tx
            .send(WriterMsg::Append {
                id,
                header,
                events,
                ack,
            })
            .await
            .map_err(|_| CoordinatorError::Shutdown)?;
        rx.await.map_err(|_| CoordinatorError::Shutdown)?
    }

    pub async fn flush(&self) -> Result<(), CoordinatorError> {
        let (ack, rx) = oneshot::channel();
        self.tx
            .send(WriterMsg::Flush { ack })
            .await
            .map_err(|_| CoordinatorError::Shutdown)?;
        rx.await.map_err(|_| CoordinatorError::Shutdown)?
    }

    /// Prepare an exact unpublished session for resume. Sends the request to
    /// the writer task, which owns the SQLite lock and the ready cache.
    pub async fn prepare(
        &self,
        id: SessionId,
        _cancel: Option<CancellationToken>,
    ) -> Result<PersistedPreparation, PrepareError> {
        let (ack, rx) = oneshot::channel();
        self.tx
            .send(WriterMsg::Prepare {
                msg: PrepareMsg { id, ack },
            })
            .await
            .map_err(|_| PrepareError::Coordinator(CoordinatorError::Shutdown))?;
        rx.await
            .map_err(|_| PrepareError::Coordinator(CoordinatorError::Shutdown))?
    }

    /// Forward one live event into the writer's merge window (used by tests
    /// that bypass the firehose).
    pub fn forward_live(&self, id: SessionId, header: SessionHeader, events: Vec<SessionEvent>) {
        let _ = self
            .live_tx
            .send(WriterMsg::LiveAppend { id, header, events });
    }

    /// Read paths are lock-guarded direct queries (WAL allows concurrent
    /// readers across processes); writes stay serialized in the writer task.
    pub fn load(&self, id: SessionId) -> Result<SessionInspection, CoordinatorError> {
        let mut db = self.shared.lock().map_err(|_| CoordinatorError::Shutdown)?;
        let stored = store::load_stored(&db, &self.store_id, &id)?.ok_or(
            CoordinatorError::Persistence(PersistenceError::NotFound(id.clone())),
        )?;
        if let Some(from_seq) = stored.torn_from {
            repair_torn_tail(&mut db, &self.store_id, &id, from_seq)?;
            let repaired = store::load_stored(&db, &self.store_id, &id)?.ok_or(
                CoordinatorError::Persistence(PersistenceError::NotFound(id.clone())),
            )?;
            return Ok(SessionInspection {
                header: repaired.header,
                events: repaired.events,
            });
        }
        Ok(SessionInspection {
            header: stored.header,
            events: stored.events,
        })
    }

    pub fn list(&self) -> Result<Vec<SessionHeader>, CoordinatorError> {
        let db = self.shared.lock().map_err(|_| CoordinatorError::Shutdown)?;
        Ok(store::list_sessions(&db)?)
    }

    pub fn remove(&self, id: SessionId) -> Result<(), CoordinatorError> {
        let db = self.shared.lock().map_err(|_| CoordinatorError::Shutdown)?;
        Ok(store::remove_session(&db, &id)?)
    }
}

async fn writer_loop(
    shared: Arc<std::sync::Mutex<rusqlite::Connection>>,
    cache: Arc<std::sync::Mutex<HashMap<SessionId, CacheEntry>>>,
    store_id: &str,
    bus: EventBus,
    window_ms: u64,
    mut rx: mpsc::Receiver<WriterMsg>,
    mut live_rx: mpsc::UnboundedReceiver<WriterMsg>,
) {
    // Fixed merge window: a deadline starts when the first append arrives;
    // all appends received before the deadline drain together. Flush drains
    // immediately and resets the window.
    let mut pending: Vec<WriterMsg> = Vec::new();
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            maybe = rx.recv() => {
                match maybe {
                    Some(WriterMsg::Flush { ack }) => {
                        drain_live_backlog(&mut live_rx, &mut pending);
                        drain_grouped(&shared, store_id, &mut pending);
                        deadline = None;
                        let _ = ack.send(Ok(()));
                    }
                    Some(WriterMsg::Prepare { msg }) => {
                        // Causal ordering (2026-09-02 cold-session drop race):
                        // live events ride the unbounded `live_rx` channel
                        // while Prepare rides `rx`; `select!` may poll Prepare
                        // first and read storage that predates events the
                        // bus already delivered. The stale load yields
                        // NotFound → a fresh session rewrites from seq 0 →
                        // the earlier batch's late drain then rejects as
                        // Discontiguous, and the firehose has no ack to
                        // surface the loss. Drain the live backlog first so
                        // Prepare always observes causally-prior events.
                        drain_live_backlog(&mut live_rx, &mut pending);
                        drain_grouped(&shared, store_id, &mut pending);
                        let result = match shared.lock() {
                            Ok(mut db) => prepare_core(
                                &mut db,
                                store_id,
                                &cache,
                                &msg.id,
                                &bus,
                            ),
                            Err(_) => Err(CoordinatorError::Shutdown),
                        };
                        let mapped = result.map_err(|error| match error {
                            CoordinatorError::Persistence(PersistenceError::NotFound(_)) => {
                                PrepareError::NotFound
                            }
                            other => PrepareError::Coordinator(other),
                        });
                        let _ = msg.ack.send(mapped);
                    }
                    Some(WriterMsg::LiveAppend {
                        id,
                        header,
                        events,
                    }) => {
                        if pending.is_empty() {
                            deadline = Some(tokio::time::Instant::now()
                                + std::time::Duration::from_millis(window_ms));
                        }
                        pending.push(WriterMsg::Append {
                            id,
                            header,
                            events,
                            ack: oneshot::channel().0,
                        });
                    }
                    Some(msg) => {
                        if pending.is_empty() {
                            deadline = Some(tokio::time::Instant::now()
                                + std::time::Duration::from_millis(window_ms));
                        }
                        pending.push(msg);
                    }
                    None => {
                        // Causally-prior live events can still sit on the
                        // firehose channel — drain them too (the Flush and
                        // Prepare arms both do), or the write-behind tail
                        // is silently dropped at teardown.
                        drain_live_backlog(&mut live_rx, &mut pending);
                        drain_grouped(&shared, store_id, &mut pending);
                        return;
                    }
                }
            }
            live = live_rx.recv() => {
                if let Some(WriterMsg::LiveAppend { id, header, events }) = live {
                    if pending.is_empty() {
                        deadline = Some(tokio::time::Instant::now()
                            + std::time::Duration::from_millis(window_ms));
                    }
                    pending.push(WriterMsg::Append {
                        id,
                        header,
                        events,
                        ack: oneshot::channel().0,
                    });
                }
            }
            _ = sleep_until(deadline) => {
                drain_grouped(&shared, store_id, &mut pending);
                deadline = None;
            }
        }
    }
}

/// Move every immediately-available live append into the pending batch.
/// `try_recv` only — we must never block the writer loop; anything not yet
/// enqueued is causally after the message currently being handled.
fn drain_live_backlog(
    live_rx: &mut mpsc::UnboundedReceiver<WriterMsg>,
    pending: &mut Vec<WriterMsg>,
) {
    while let Ok(msg) = live_rx.try_recv() {
        if let WriterMsg::LiveAppend { id, header, events } = msg {
            // No deadline bookkeeping needed: both call sites drain_grouped
            // immediately after, emptying `pending`; a stale deadline would
            // at worst fire one no-op drain later.
            pending.push(WriterMsg::Append {
                id,
                header,
                events,
                ack: oneshot::channel().0,
            });
        }
    }
}

fn prepare_core(
    db: &mut rusqlite::Connection,
    store_id: &str,
    cache: &Arc<std::sync::Mutex<HashMap<SessionId, CacheEntry>>>,
    id: &SessionId,
    bus: &EventBus,
) -> Result<PersistedPreparation, CoordinatorError> {
    let current = store::read_stored_revision(db, store_id, id)?.ok_or(
        CoordinatorError::Persistence(PersistenceError::NotFound(id.clone())),
    )?;
    // Revision-stable cache reuse: hit only when the durable revision is
    // unchanged (an external writer would have bumped it). The entry is
    // removed on hit so the preparation is exclusive until release/attach.
    let mut cache_guard = cache.lock().unwrap();
    if let Some(entry) = cache_guard.remove(id) {
        if entry.revision == current {
            let session = entry.session;
            let cache_session = session.clone();
            let cache = Arc::clone(cache);
            let cache_id = id.clone();
            let cache_revision = current;
            return Ok(PersistedPreparation::new(
                session,
                Box::new(move |reusable| {
                    if reusable {
                        cache.lock().unwrap().insert(
                            cache_id,
                            CacheEntry {
                                session: cache_session,
                                revision: cache_revision,
                            },
                        );
                        trim_cache(&mut cache.lock().unwrap());
                    }
                }),
            ));
        }
        cache_guard.insert(id.clone(), entry);
    }
    drop(cache_guard);
    let stored = store::load_stored(db, store_id, id)?.ok_or(CoordinatorError::Persistence(
        PersistenceError::NotFound(id.clone()),
    ))?;
    if let Some(from_seq) = stored.torn_from {
        repair_torn_tail(db, store_id, id, from_seq)?;
        return prepare_core(db, store_id, cache, id, bus);
    }
    let closers = crate::repair::closers_for(&stored.events);
    if !closers.is_empty() {
        // Crash repair is durable before the preparation is published.
        store::append_batch(db, &stored.header, &closers, true)?;
    }
    let mut balanced = stored.events;
    balanced.extend(closers);
    // end-seed: persist the durable lineage boundary once (repeated resume
    // sees the marker and does not append again).
    let ends_with_marker = balanced
        .last()
        .map(|event| event.event_type.as_str() == "session/end-seed")
        .unwrap_or(false);
    if !ends_with_marker {
        let marker = SessionEvent {
            seq: balanced.len() as u64,
            time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            event_type: "session/end-seed".into(),
            data: starriver_kernel::json::JsonValue::Null,
            surface_op: None,
            source_event_seqs: Vec::new(),
        };
        store::append_batch(db, &stored.header, std::slice::from_ref(&marker), true)?;
        balanced.push(marker);
    }
    let session = Session::new(id.clone(), bus.clone())
        .with_header(stored.header.clone())
        .map_err(|e| CoordinatorError::Persistence(PersistenceError::Corruption(e.to_string())))?
        .with_seed(balanced)
        .map_err(|e| CoordinatorError::Persistence(PersistenceError::Corruption(e.to_string())))?;
    let cache_session = session.clone();
    let cache = Arc::clone(cache);
    let cache_id = id.clone();
    let cache_revision = current;
    Ok(PersistedPreparation::new(
        session,
        Box::new(move |reusable| {
            if reusable {
                cache.lock().unwrap().insert(
                    cache_id,
                    CacheEntry {
                        session: cache_session,
                        revision: cache_revision,
                    },
                );
                trim_cache(&mut cache.lock().unwrap());
            }
        }),
    ))
}

/// Truncate a torn physical tail and bump the session revision so concurrent
/// writers and caches observe the repair.
fn repair_torn_tail(
    db: &mut rusqlite::Connection,
    _store_id: &str,
    id: &SessionId,
    from_seq: u64,
) -> Result<(), CoordinatorError> {
    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| CoordinatorError::Persistence(PersistenceError::Io(e.to_string())))?;
    let result = (|| {
        store::truncate_torn_tail(&tx, id, from_seq)?;
        tx.execute(
            "UPDATE sessions SET revision = revision + 1 WHERE id = ?1",
            [&id.0],
        )
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
        Ok(())
    })();
    match result {
        Ok(()) => tx
            .commit()
            .map_err(|e| CoordinatorError::Persistence(PersistenceError::Io(e.to_string()))),
        Err(error) => {
            let _ = tx.rollback();
            Err(CoordinatorError::Persistence(error))
        }
    }
}

fn trim_cache(cache: &mut HashMap<SessionId, CacheEntry>) {
    while cache.len() > DEFAULT_PREPARED_CACHE_SIZE {
        if let Some(oldest) = cache.keys().next().cloned() {
            cache.remove(&oldest);
        } else {
            break;
        }
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

fn drain_grouped(
    shared: &Arc<std::sync::Mutex<rusqlite::Connection>>,
    store_id: &str,
    pending: &mut Vec<WriterMsg>,
) {
    let appends = std::mem::take(pending)
        .into_iter()
        .filter_map(|msg| match msg {
            WriterMsg::Append {
                id,
                header,
                events,
                ack,
            } => Some((id, header, events, ack)),
            WriterMsg::Flush { .. } => None,
            WriterMsg::Prepare { .. } => None,
            WriterMsg::LiveAppend { .. } => None,
        })
        .collect::<Vec<_>>();
    // Group by session preserving arrival order.
    let mut groups: Vec<GroupedBatch> = Vec::new();
    for (id, header, events, ack) in appends {
        if let Some((_, _, group_events, acks)) =
            groups.iter_mut().find(|(gid, _, _, _)| *gid == id)
        {
            group_events.extend(events);
            acks.push(ack);
            continue;
        }
        groups.push((id, header, events, vec![ack]));
    }
    for (id, header, events, acks) in groups {
        let result = {
            let Ok(mut db) = shared.lock() else {
                for ack in acks {
                    let _ = ack.send(Err(CoordinatorError::Shutdown));
                }
                return;
            };
            // Distinguish "not materialized" from "corrupt" (BUG: the
            // `.ok()` swallow turned a mid-log Corruption into a silent
            // None → INSERT hit the primary key → the caller saw a
            // meaningless UNIQUE-constraint Io error).
            let materialized = match store::load_stored(&db, store_id, &id) {
                Ok(stored) => stored.is_some(),
                Err(error) => {
                    // Surface the corruption: to the first live ack, or at
                    // least to the log (the old `.ok()` swallow reported a
                    // meaningless UNIQUE-constraint error instead).
                    tracing::error!(session = %id.0, error = %error,
                        "append blocked by stored-log corruption");
                    // Ack EVERY caller (the `break` fed only the first ack —
                    // often a closed dummy channel, so real callers saw a
                    // misleading Shutdown) and keep draining the remaining
                    // groups: one session's corrupt row must not discard
                    // unrelated sessions' co-batched events.
                    for ack in acks {
                        let _ = ack.send(Err(CoordinatorError::Persistence(
                            PersistenceError::Corruption(error.to_string()),
                        )));
                    }
                    continue;
                }
            };
            let mut result = store::append_batch(&mut db, &header, &events, materialized);
            // BUG-154 (2026-10-14, session bc6ff9a5…): a firehose span
            // (events 44023–44161, the escalated-retry compaction window)
            // never reached the writer; every later batch then failed the
            // contiguity check forever — 35 silent rejections, and the
            // turn's in-memory tail died with the process. Failing closed
            // here forfeits the WHOLE rest of the session for one missed
            // span. Firehose batches carry no caller ack, so bridge the gap
            // with one explicit `session/gap` tombstone and retry: the
            // ledger stays appendable and the loss becomes bounded and
            // visible in the trace instead of silently orphaning the
            // session. Never fabricate events under a real caller's ack.
            if let Err(PersistenceError::Discontiguous { expected, got, .. }) = &result {
                let firehose_only = acks.iter().all(|ack| ack.is_closed());
                if firehose_only && got > expected {
                    // One tombstone per missing seq: append_batch enforces
                    // strict +1 contiguity, so the bridge itself must be a
                    // contiguous run. The first tombstone carries the span
                    // metadata; the rest are bare markers.
                    let (from, until) = (*expected, got - 1);
                    let time = events.first().map(|e| e.time).unwrap_or(0);
                    let bridge: Vec<SessionEvent> = (from..=until)
                        .map(|seq| SessionEvent {
                            seq,
                            time,
                            event_type: "session/gap".into(),
                            data: starriver_kernel::json::JsonValue::Object(vec![
                                (
                                    "reason".into(),
                                    starriver_kernel::json::JsonValue::String(
                                        "events never reached the persistence writer; bridged to keep the ledger appendable".into(),
                                    ),
                                ),
                                ("seq".into(), starriver_kernel::json::JsonValue::UInt(seq)),
                                ("spanFrom".into(), starriver_kernel::json::JsonValue::UInt(from)),
                                ("spanUntil".into(), starriver_kernel::json::JsonValue::UInt(until)),
                            ]),
                            surface_op: None,
                            source_event_seqs: Vec::new(),
                        })
                        .collect();
                    match store::append_batch(&mut db, &header, &bridge, materialized) {
                        Ok(()) => {
                            tracing::error!(
                                session = %id.0,
                                from = from,
                                until = until,
                                lost = until - from + 1,
                                "persistence gap bridged with session/gap tombstones; \
                                 events in the span are lost"
                            );
                            result = store::append_batch(&mut db, &header, &events, materialized);
                        }
                        Err(bridge_error) => {
                            tracing::error!(
                                session = %id.0,
                                error = ?bridge_error,
                                "session/gap bridge failed; batch stays rejected"
                            );
                        }
                    }
                }
            }
            result
        };
        // Firehose appends carry no ack (write-behind), so a failed batch is
        // otherwise silent data loss. LiveAppend acks are dummy senders; only
        // real callers get the error through their oneshot.
        if let Err(error) = &result {
            if acks.is_empty() || acks.iter().all(|ack| ack.is_closed()) {
                tracing::warn!(
                    session = %id.0,
                    error = ?error,
                    first_seq = events.first().map(|e| e.seq).unwrap_or(0),
                    "write-behind append failed with no live ack to receive it"
                );
            }
        }
        for ack in acks {
            let _ = ack.send(result.clone().map_err(CoordinatorError::Persistence));
        }
    }
}

#[cfg(test)]
mod tests {
    /// Tests wait out the write-batch window, so keep it tiny — the default
    /// 10s debounce is production tuning, not a test invariant.
    const TEST_WINDOW_MS: u64 = 25;
    use super::*;
    use starriver_kernel::json::JsonValue;

    fn header(id: &str) -> SessionHeader {
        let mut h = SessionHeader::new(SessionId(id.into()));
        h.cwd = Some("/tmp".into());
        h
    }

    fn event(seq: u64, event_type: &str) -> SessionEvent {
        SessionEvent {
            seq,
            time: 1,
            event_type: event_type.into(),
            data: JsonValue::Null,
            surface_op: None,
            source_event_seqs: Vec::new(),
        }
    }

    fn open_db(path: &std::path::Path) -> (rusqlite::Connection, String) {
        let db = rusqlite::Connection::open(path).unwrap();
        crate::schema::open_owned(&db, path).unwrap();
        let store_id = db
            .query_row("SELECT store_id FROM persistence_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        (db, store_id)
    }

    #[tokio::test]
    async fn flush_drains_buffered_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let (db, store_id) = open_db(&path);
        let coordinator = WriteCoordinator::spawn(
            db,
            store_id.clone(),
            cordis_rslite::EventBus::new(),
            TEST_WINDOW_MS,
            16,
        );
        coordinator
            .append(
                SessionId("s1".into()),
                header("s1"),
                vec![event(0, "turn/start"), event(1, "user/message")],
            )
            .await
            .unwrap();
        coordinator.flush().await.unwrap();
        let read = rusqlite::Connection::open(&path).unwrap();
        let stored = store::load_stored(&read, &store_id, &SessionId("s1".into()))
            .unwrap()
            .unwrap();
        assert_eq!(stored.events.len(), 2);
    }

    #[tokio::test]
    async fn discontiguous_append_rejects_and_keeps_storage_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let (db, store_id) = open_db(&path);
        let coordinator = WriteCoordinator::spawn(
            db,
            store_id.clone(),
            cordis_rslite::EventBus::new(),
            TEST_WINDOW_MS,
            16,
        );
        let error = coordinator
            .append(
                SessionId("s1".into()),
                header("s1"),
                vec![event(5, "turn/start")],
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            CoordinatorError::Persistence(PersistenceError::Discontiguous { .. })
        ));
        coordinator.flush().await.unwrap();
        let read = rusqlite::Connection::open(&path).unwrap();
        assert!(
            store::load_stored(&read, &store_id, &SessionId("s1".into()))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn concurrent_flushes_share_the_same_barrier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let (db, store_id) = open_db(&path);
        let coordinator = Arc::new(WriteCoordinator::spawn(
            db,
            store_id.clone(),
            cordis_rslite::EventBus::new(),
            TEST_WINDOW_MS,
            16,
        ));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = Arc::clone(&coordinator);
            handles.push(tokio::spawn(async move { c.flush().await.unwrap() }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
    }

    #[tokio::test]
    async fn prepare_returns_balanced_session_with_end_seed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let (db, store_id) = open_db(&path);
        let coordinator = WriteCoordinator::spawn(
            db,
            store_id.clone(),
            cordis_rslite::EventBus::new(),
            TEST_WINDOW_MS,
            16,
        );
        let id = SessionId("s1".into());
        // Open turn (no turn/end) so repair synthesizes closers.
        coordinator
            .append(
                id.clone(),
                header("s1"),
                vec![event(0, "turn/start"), event(1, "assistant/message")],
            )
            .await
            .unwrap();
        coordinator.flush().await.unwrap();
        let preparation = coordinator.prepare(id.clone(), None).await.unwrap();
        let events = preparation.session.events();
        assert!(events.iter().any(|e| e.event_type == "turn/end"));
        assert!(events.iter().any(|e| e.event_type == "session/end-seed"));
        preparation.attach();
        // Second prepare on the same id is a fresh NotFound-free path after
        // attach? No: attach consumed it; a second prepare reloads storage.
        let again = coordinator.prepare(id.clone(), None).await.unwrap();
        assert!(again
            .session
            .events()
            .iter()
            .any(|e| e.event_type == "session/end-seed"));
        again.attach();
    }

    #[tokio::test]
    async fn prepare_sees_live_appends_forwarded_before_it() {
        // Regression (2026-09-02 cold-session drop race): forward_live rides
        // the unbounded channel while prepare rides the bounded one, and
        // select! could poll Prepare first — loading storage that predates
        // already-delivered live events, seeing NotFound, and letting the
        // resumed incarnation's writes fail as Discontiguous with no ack to
        // surface the loss. Prepare must drain the live backlog first. The
        // loop makes the pre-fix interleaving near-certain to appear.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let (db, store_id) = open_db(&path);
        let coordinator = WriteCoordinator::spawn(
            db,
            store_id.clone(),
            cordis_rslite::EventBus::new(),
            TEST_WINDOW_MS,
            16,
        );
        for round in 0..20 {
            let name = format!("s{round}");
            let id = SessionId(name.clone());
            coordinator.forward_live(
                id.clone(),
                header(&name),
                vec![event(0, "turn/start"), event(1, "user/message")],
            );
            // No flush: prepare alone must observe the forwarded events.
            let preparation = match coordinator.prepare(id.clone(), None).await {
                Ok(preparation) => preparation,
                Err(PrepareError::NotFound) => {
                    panic!("prepare raced ahead of live appends (round {round})")
                }
                Err(other) => panic!("unexpected prepare error: {other:?}"),
            };
            assert!(
                preparation
                    .session
                    .events()
                    .iter()
                    .any(|e| e.event_type == "turn/start"),
                "round {round}: persisted events missing"
            );
            preparation.attach();
        }
    }

    #[tokio::test]
    async fn live_appends_after_prepare_round_trip() {
        // Host resume flow: turn 1 persists, prepare rehydrates the session,
        // the resumed agent keeps appending on the SAME session object (bus
        // emits → firehose), and those events must reach durable storage.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let (db, store_id) = open_db(&path);
        let bus = cordis_rslite::EventBus::new();
        let coordinator = WriteCoordinator::spawn(db, store_id.clone(), bus, TEST_WINDOW_MS, 16);
        let id = SessionId("s1".into());
        coordinator
            .append(
                id.clone(),
                header("s1"),
                vec![event(0, "turn/start"), event(1, "turn/end")],
            )
            .await
            .unwrap();
        coordinator.flush().await.unwrap();
        let preparation = coordinator.prepare(id.clone(), None).await.unwrap();
        let session = preparation.session.clone();
        preparation.attach();
        session
            .append(
                "sandbox/mode",
                starriver_kernel::json::JsonValue::Object(vec![(
                    "mode".into(),
                    starriver_kernel::json::JsonValue::String("workspace-write".into()),
                )]),
            )
            .unwrap();
        session.append("turn/start", JsonValue::Null).unwrap();
        coordinator.flush().await.unwrap();
        let stored = coordinator.load(id.clone()).unwrap();
        assert!(
            stored.events.iter().any(|e| e.event_type == "sandbox/mode"
                && e.data.as_str_at("mode") == Some("workspace-write")),
            "post-prepare live appends must persist; log tail: {:?}",
            stored
                .events
                .iter()
                .map(|e| e.event_type.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn merge_window_deadline_drains_without_flush() {
        // The 200ms merge window must flush on its own — no explicit Flush or
        // Prepare required. A window that only drains on other messages loses
        // the tail of every idle session.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let (db, store_id) = open_db(&path);
        let coordinator =
            WriteCoordinator::spawn(db, store_id.clone(), cordis_rslite::EventBus::new(), 50, 16);
        let id = SessionId("s1".into());
        coordinator.forward_live(id.clone(), header("s1"), vec![event(0, "turn/start")]);
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let stored = coordinator.load(id.clone()).unwrap();
        assert_eq!(
            stored.events.len(),
            1,
            "merge-window deadline must drain without an explicit flush"
        );
    }

    #[tokio::test]
    async fn prepare_missing_session_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let (db, store_id) = open_db(&path);
        let coordinator = WriteCoordinator::spawn(
            db,
            store_id.clone(),
            cordis_rslite::EventBus::new(),
            TEST_WINDOW_MS,
            16,
        );
        let result = coordinator.prepare(SessionId("missing".into()), None).await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("prepare must fail for a missing session"),
        };
        assert!(matches!(error, crate::prepare::PrepareError::NotFound));
    }

    #[tokio::test]
    async fn load_repairs_torn_tail_durably() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let (db, store_id) = open_db(&path);
        let coordinator = WriteCoordinator::spawn(
            db,
            store_id.clone(),
            cordis_rslite::EventBus::new(),
            TEST_WINDOW_MS,
            16,
        );
        let id = SessionId("s1".into());
        coordinator
            .append(
                id.clone(),
                header("s1"),
                vec![event(0, "turn/start"), event(1, "user/message")],
            )
            .await
            .unwrap();
        coordinator.flush().await.unwrap();
        // Corrupt the physical tail row behind the coordinator's back.
        {
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute("UPDATE events SET data = '{torn' WHERE seq = 1", [])
                .unwrap();
        }
        let inspection = coordinator.load(id.clone()).unwrap();
        assert_eq!(inspection.events.len(), 1);
        // The repair is durable: a fresh read no longer sees the torn row.
        let read = rusqlite::Connection::open(&path).unwrap();
        let stored = store::load_stored(&read, &store_id, &id).unwrap().unwrap();
        assert_eq!(stored.events.len(), 1);
    }

    #[tokio::test]
    async fn prepare_reuses_revision_stable_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let (db, store_id) = open_db(&path);
        let coordinator = WriteCoordinator::spawn(
            db,
            store_id.clone(),
            cordis_rslite::EventBus::new(),
            TEST_WINDOW_MS,
            16,
        );
        let id = SessionId("s1".into());
        coordinator
            .append(id.clone(), header("s1"), vec![event(0, "turn/start")])
            .await
            .unwrap();
        coordinator.flush().await.unwrap();
        let first = coordinator.prepare(id.clone(), None).await.unwrap();
        let first_types = first
            .session
            .events()
            .into_iter()
            .map(|e| e.event_type)
            .collect::<Vec<_>>();
        // Dropping without attach returns the source to the cache (reusable).
        drop(first);
        let second = coordinator.prepare(id.clone(), None).await.unwrap();
        let second_types = second
            .session
            .events()
            .into_iter()
            .map(|e| e.event_type)
            .collect::<Vec<_>>();
        assert_eq!(first_types, second_types);
        second.attach();
    }

    /// BUG-154 recovery: a firehose span that never reached the writer must
    /// not orphan the session — the gap gets one `session/gap` tombstone and
    /// later batches land contiguously.
    #[tokio::test]
    async fn firehose_gap_bridges_with_tombstone_and_session_stays_appendable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let (db, store_id) = open_db(&path);
        let coordinator = WriteCoordinator::spawn(
            db,
            store_id.clone(),
            cordis_rslite::EventBus::new(),
            TEST_WINDOW_MS,
            16,
        );
        let id = SessionId("g".into());
        let head = header("g");
        coordinator.forward_live(
            id.clone(),
            head.clone(),
            vec![event(0, "turn/start"), event(1, "assistant/message")],
        );
        coordinator.flush().await.unwrap();
        // Events 2..=6 never reached the writer; the next batch starts at 7.
        coordinator.forward_live(id.clone(), head.clone(), vec![event(7, "user/message")]);
        coordinator.flush().await.unwrap();
        let read = rusqlite::Connection::open(&path).unwrap();
        let stored = store::load_stored(&read, &store_id, &id)
            .unwrap()
            .expect("session persisted");
        // The span 2..=6 is bridged by per-seq tombstones; the batch lands after.
        assert_eq!(stored.events.len(), 8);
        for (index, stored_event) in stored.events.iter().enumerate().skip(2).take(5) {
            assert_eq!(stored_event.seq, index as u64);
            assert_eq!(stored_event.event_type, "session/gap");
        }
        assert_eq!(stored.events[7].event_type, "user/message");
        assert_eq!(stored.events[7].seq, 7);
    }

    async fn append_batches_group_by_session_independently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let (db, store_id) = open_db(&path);
        let coordinator = WriteCoordinator::spawn(
            db,
            store_id.clone(),
            cordis_rslite::EventBus::new(),
            TEST_WINDOW_MS,
            16,
        );
        coordinator
            .append(
                SessionId("a".into()),
                header("a"),
                vec![event(0, "turn/start")],
            )
            .await
            .unwrap();
        coordinator
            .append(
                SessionId("b".into()),
                header("b"),
                vec![event(0, "turn/start")],
            )
            .await
            .unwrap();
        coordinator.flush().await.unwrap();
        let read = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(store::list_sessions(&read).unwrap().len(), 2);
        assert_eq!(
            store::load_stored(&read, &store_id, &SessionId("a".into()))
                .unwrap()
                .unwrap()
                .events
                .len(),
            1
        );
        assert_eq!(
            store::load_stored(&read, &store_id, &SessionId("b".into()))
                .unwrap()
                .unwrap()
                .events
                .len(),
            1
        );
    }
}
