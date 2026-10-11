//! Durable session persistence seam.

use cordis_rslite::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::session::{SessionEvent, SessionHeader, SessionId};

/// Validated durable view returned by [`SessionPersistence::load`].
#[derive(Debug, Clone)]
pub struct SessionInspection {
    pub header: SessionHeader,
    /// A log ending on a balanced `turn/end` (or empty). Repair closers are
    /// included; the torn tail is already truncated.
    pub events: Vec<SessionEvent>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum PersistenceError {
    #[error("session `{0}` not found in durable storage")]
    NotFound(SessionId),
    #[error("stored session failed validation: {0}")]
    Corruption(String),
    #[error("stored session format is unsupported: {0}")]
    FormatUnsupported(String),
    #[error("session `{0}` is already live")]
    AlreadyLive(SessionId),
    #[error("append seq mismatch for `{id}`: expected {expected} at index {index}, got {got}")]
    Discontiguous {
        id: SessionId,
        expected: u64,
        got: u64,
        index: usize,
    },
    #[error("database busy after retry deadline")]
    Busy,
    #[error("storage error: {0}")]
    Io(String),
    #[error("operation canceled")]
    Canceled,
}

/// Object-safe durable storage seam. Backends are plugins; the kernel only
/// owns this contract. `prepare` intentionally stays out of this trait (the
/// default crate owns the guarded preparation), matching the design spec.
pub trait SessionPersistence: Send + Sync {
    /// Register a new session's metadata. May defer the physical row until
    /// the first [`Self::append`] (lazy materialization); a created-but-never
    /// appended session is absent from [`Self::list`].
    fn create(&self, header: SessionHeader) -> BoxFuture<Result<(), PersistenceError>>;

    /// Durably persist a contiguous batch. The first event's `seq` must
    /// equal the stored next seq; non-contiguous batches reject and are
    /// retained by the coordinator.
    fn append(
        &self,
        id: SessionId,
        events: Vec<SessionEvent>,
        cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<(), PersistenceError>>;

    /// Load an immutable balanced view, committing any required cold repair
    /// (truncate torn tail, synthesize closers) as a durable side effect.
    fn load(
        &self,
        id: SessionId,
        cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<SessionInspection, PersistenceError>>;

    /// List metadata for every materialized session.
    fn list(
        &self,
        cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<Vec<SessionHeader>, PersistenceError>>;

    /// Remove a session and its events (cascade).
    fn remove(
        &self,
        id: SessionId,
        cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<(), PersistenceError>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A minimal probe implementing the seam; exercises every required
    /// signature and the object-safety of the trait.
    struct Probe;

    impl SessionPersistence for Probe {
        fn create(&self, _header: SessionHeader) -> BoxFuture<Result<(), PersistenceError>> {
            Box::pin(async { Ok(()) })
        }
        fn append(
            &self,
            _id: SessionId,
            _events: Vec<SessionEvent>,
            _cancel: Option<CancellationToken>,
        ) -> BoxFuture<Result<(), PersistenceError>> {
            Box::pin(async { Ok(()) })
        }
        fn load(
            &self,
            _id: SessionId,
            _cancel: Option<CancellationToken>,
        ) -> BoxFuture<Result<SessionInspection, PersistenceError>> {
            Box::pin(async { Err(PersistenceError::NotFound(SessionId("missing".into()))) })
        }
        fn list(
            &self,
            _cancel: Option<CancellationToken>,
        ) -> BoxFuture<Result<Vec<SessionHeader>, PersistenceError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn remove(
            &self,
            _id: SessionId,
            _cancel: Option<CancellationToken>,
        ) -> BoxFuture<Result<(), PersistenceError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn seam_is_object_safe_and_callable() {
        let dyn_seam: Arc<dyn SessionPersistence> = Arc::new(Probe);
        assert!(dyn_seam.load(SessionId("x".into()), None).await.is_err());
        assert!(dyn_seam.list(None).await.unwrap().is_empty());
    }

    #[test]
    fn error_messages_are_stable() {
        let error = PersistenceError::Discontiguous {
            id: SessionId("s".into()),
            expected: 2,
            got: 5,
            index: 0,
        };
        let text = error.to_string();
        assert!(text.contains("expected 2"));
        assert!(text.contains("got 5"));
    }

    #[test]
    fn error_messages_cover_every_variant() {
        let cases = [
            (
                PersistenceError::NotFound(SessionId("x".into())),
                "not found",
            ),
            (PersistenceError::Corruption("bad row".into()), "validation"),
            (
                PersistenceError::FormatUnsupported("v9".into()),
                "unsupported",
            ),
            (
                PersistenceError::AlreadyLive(SessionId("x".into())),
                "already live",
            ),
            (PersistenceError::Busy, "busy"),
            (PersistenceError::Io("disk".into()), "storage error"),
            (PersistenceError::Canceled, "canceled"),
        ];
        for (error, needle) in cases {
            let text = error.to_string();
            assert!(
                text.to_ascii_lowercase().contains(needle),
                "expected `{text}` to contain `{needle}`"
            );
        }
    }
}
