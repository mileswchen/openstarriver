//! Prepared unpublished session: guarded reserve, revision-stable cache
//! reuse, and Drop release. Intentionally NOT a four-phase state
//! machine: process-local exclusivity comes from guard transfer; cross-process
//! consistency comes from durable revision checks at commit time.

use starriver_kernel::session::{Session, SessionId};
use tokio::sync::oneshot;

use crate::coordinator::CoordinatorError;

/// Preparation errors surfaced to resume callers. `NotFound` is the only
/// recoverable one (fall back to create); everything else stays loud.
#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error("session not found")]
    NotFound,
    #[error(transparent)]
    Coordinator(#[from] CoordinatorError),
}

/// One exclusively held unpublished session and its committed persistence
/// state. Publishing consumes the guard; dropping without publishing returns
/// the reservation to the ready cache (when unchanged) or discards it.
pub struct PersistedPreparation {
    pub session: Session,
    release: Option<Box<dyn FnOnce(bool) + Send>>,
    session_length: u64,
}

impl PersistedPreparation {
    pub fn new(session: Session, release: Box<dyn FnOnce(bool) + Send>) -> Self {
        let session_length = session.events().len() as u64;
        Self {
            session,
            release: Some(release),
            session_length,
        }
    }

    /// Consume the reservation after publication: never returns to cache.
    pub fn attach(mut self) {
        if let Some(release) = self.release.take() {
            release(false);
        }
    }
}

impl Drop for PersistedPreparation {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let reusable = self.session.events().len() as u64 == self.session_length;
            release(reusable);
        }
    }
}

/// Prepare request/ack pair owned by the coordinator.
pub struct PrepareMsg {
    pub id: SessionId,
    pub ack: oneshot::Sender<Result<PersistedPreparation, PrepareError>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn drop_releases_with_reusable_flag_when_unchanged() {
        let bus = cordis_rslite::EventBus::new();
        let session = Session::new(SessionId("s".into()), bus);
        let released = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&released);
        let preparation = PersistedPreparation::new(
            session,
            Box::new(move |reusable| {
                assert!(reusable);
                flag.store(true, Ordering::SeqCst);
            }),
        );
        drop(preparation);
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn attach_never_returns_to_cache() {
        let bus = cordis_rslite::EventBus::new();
        let session = Session::new(SessionId("s".into()), bus);
        let released = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&released);
        let preparation = PersistedPreparation::new(
            session,
            Box::new(move |reusable| {
                assert!(!reusable);
                flag.store(true, Ordering::SeqCst);
            }),
        );
        preparation.attach();
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn drop_after_growth_is_not_reusable() {
        let bus = cordis_rslite::EventBus::new();
        let session = Session::new(SessionId("s".into()), bus);
        let released = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&released);
        let preparation = PersistedPreparation::new(
            session,
            Box::new(move |reusable| {
                assert!(!reusable, "grown session must not return to cache");
                flag.store(true, Ordering::SeqCst);
            }),
        );
        // Simulate live growth after construction: append an event.
        preparation
            .session
            .append("turn/start", starriver_kernel::json::JsonValue::Null)
            .unwrap();
        drop(preparation);
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn attach_then_drop_does_not_release_again() {
        let bus = cordis_rslite::EventBus::new();
        let session = Session::new(SessionId("s".into()), bus);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let preparation = PersistedPreparation::new(
            session,
            Box::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        );
        preparation.attach();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
