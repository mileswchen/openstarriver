//! Durable session persistence for Starriver.

use std::path::PathBuf;
use std::sync::Arc;

use cordis_rslite::BoxFuture;
use starriver_kernel::persistence::{PersistenceError, SessionInspection, SessionPersistence};
use starriver_kernel::session::{SessionEvent, SessionHeader, SessionId};
use tokio_util::sync::CancellationToken;

pub mod backend;
pub mod chunk_rows;
pub mod compression;
pub mod coordinator;
pub mod inspect;
pub mod cold_compress;
pub mod jsonl;
pub mod prepare;
pub mod query;
pub mod repair;
pub mod schema;
pub mod store;

use coordinator::WriteCoordinator;

/// Concrete persistence service: owned SQLite database plus the write-behind
/// coordinator. Register as a cordis service at the composition root.
pub struct SqliteSessionPersistence {
    coordinator: Arc<WriteCoordinator>,
}

impl SqliteSessionPersistence {
    pub fn open(path: PathBuf) -> Result<Self, String> {
        Self::open_with_bus(path, cordis_rslite::EventBus::new())
    }

    /// Open with the runtime's event bus so live `session/event` occurrences
    /// on that bus are write-behind persisted by the coordinator.
    pub fn open_with_bus(path: PathBuf, bus: cordis_rslite::EventBus) -> Result<Self, String> {
        let db = rusqlite::Connection::open(&path).map_err(|e| e.to_string())?;
        schema::open_owned(&db, &path)?;
        let store_id: String = db
            .query_row("SELECT store_id FROM persistence_state", [], |row| {
                row.get(0)
            })
            .map_err(|e| e.to_string())?;
        let coordinator = WriteCoordinator::spawn(
            db,
            store_id,
            bus,
            coordinator::DEFAULT_WRITE_BATCH_MAX_DELAY_MS,
            coordinator::DEFAULT_CHANNEL_CAPACITY,
        );
        Ok(Self {
            coordinator: Arc::new(coordinator),
        })
    }

    pub fn coordinator(&self) -> &Arc<WriteCoordinator> {
        &self.coordinator
    }

    /// Prepare an exact unpublished session for resume (default-crate
    /// extension interface, deliberately outside the kernel seam).
    pub async fn prepare(
        &self,
        id: SessionId,
        cancel: Option<CancellationToken>,
    ) -> Result<prepare::PersistedPreparation, prepare::PrepareError> {
        self.coordinator.prepare(id, cancel).await
    }

    /// Explicit durability barrier: drain all buffered appends and wait for
    /// the writer task to reach quiescence.
    pub async fn flush(&self) -> Result<(), PersistenceError> {
        self.coordinator.flush().await.map_err(|e| match e {
            coordinator::CoordinatorError::Shutdown => {
                PersistenceError::Io("coordinator shut down".into())
            }
            coordinator::CoordinatorError::Persistence(p) => p,
        })
    }
}

impl SessionPersistence for SqliteSessionPersistence {
    fn create(&self, _header: SessionHeader) -> BoxFuture<Result<(), PersistenceError>> {
        // Lazy materialization: nothing durable until the first append.
        Box::pin(async { Ok(()) })
    }

    fn append(
        &self,
        id: SessionId,
        events: Vec<SessionEvent>,
        cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<(), PersistenceError>> {
        // The public append route is write-through: enqueue then flush so the
        // future resolves only after durability.
        let coordinator = Arc::clone(&self.coordinator);
        Box::pin(async move {
            if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                return Err(PersistenceError::Canceled);
            }
            let header = SessionHeader::new(id.clone());
            coordinator
                .append(id.clone(), header, events)
                .await
                .map_err(|e| match e {
                    coordinator::CoordinatorError::Shutdown => {
                        PersistenceError::Io("coordinator shut down".into())
                    }
                    coordinator::CoordinatorError::Persistence(p) => p,
                })?;
            coordinator
                .flush()
                .await
                .map_err(|e| PersistenceError::Io(e.to_string()))
        })
    }

    fn load(
        &self,
        id: SessionId,
        _cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<SessionInspection, PersistenceError>> {
        let coordinator = Arc::clone(&self.coordinator);
        Box::pin(async move {
            coordinator.load(id).map_err(|e| match e {
                coordinator::CoordinatorError::Shutdown => {
                    PersistenceError::Io("coordinator shut down".into())
                }
                coordinator::CoordinatorError::Persistence(p) => p,
            })
        })
    }

    fn list(
        &self,
        _cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<Vec<SessionHeader>, PersistenceError>> {
        let coordinator = Arc::clone(&self.coordinator);
        Box::pin(async move {
            coordinator.list().map_err(|e| match e {
                coordinator::CoordinatorError::Shutdown => {
                    PersistenceError::Io("coordinator shut down".into())
                }
                coordinator::CoordinatorError::Persistence(p) => p,
            })
        })
    }

    fn remove(
        &self,
        id: SessionId,
        _cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<(), PersistenceError>> {
        let coordinator = Arc::clone(&self.coordinator);
        Box::pin(async move {
            coordinator.remove(id).map_err(|e| match e {
                coordinator::CoordinatorError::Shutdown => {
                    PersistenceError::Io("coordinator shut down".into())
                }
                coordinator::CoordinatorError::Persistence(p) => p,
            })
        })
    }
}
