use std::sync::{Arc, Mutex};

type Cleanup = Box<dyn FnOnce() -> crate::BoxFuture<()> + Send>;

/// A single-shot awaitable cleanup. Wrap synchronous cleanup with [`EffectHandle::new`], asynchronous
/// cleanup with [`EffectHandle::new_async`]; both run via [`EffectHandle::dispose_async`].
#[must_use = "EffectHandle must be disposed with dispose_async() or adopted by a Fiber"]
#[derive(Clone)]
pub struct EffectHandle {
    /// Shared slot so the handle can be cloned (contexts track a copy for
    /// scope teardown) while the cleanup still runs exactly once.
    cleanup: Option<Arc<Mutex<Option<Cleanup>>>>,
}

impl EffectHandle {
    /// Wrap a synchronous cleanup.
    pub fn new(f: impl FnOnce() + Send + 'static) -> Self {
        Self {
            cleanup: Some(Arc::new(Mutex::new(Some(Box::new(move || {
                Box::pin(async move { f() })
            }))))),
        }
    }

    /// Wrap an asynchronous cleanup.
    pub fn new_async(f: impl FnOnce() -> crate::BoxFuture<()> + Send + 'static) -> Self {
        Self {
            cleanup: Some(Arc::new(Mutex::new(Some(Box::new(f))))),
        }
    }

    /// A placeholder handle without cleanup; `dispose_async` is a no-op.
    pub fn none() -> Self {
        Self { cleanup: None }
    }

    pub fn has_cleanup(&self) -> bool {
        self.cleanup
            .as_ref()
            .is_some_and(|slot| slot.lock().unwrap().is_some())
    }

    pub fn into_cleanup(mut self) -> Option<Cleanup> {
        self.cleanup
            .take()
            .and_then(|slot| slot.lock().unwrap().take())
    }

    /// Consume the handle without running its cleanup and without emitting the drop warning.
    pub fn forget(mut self) {
        self.cleanup.take();
    }

    pub async fn dispose_async(mut self) {
        let cleanup = self
            .cleanup
            .take()
            .and_then(|slot| slot.lock().unwrap().take());
        if let Some(f) = cleanup {
            f().await;
        }
    }
}

impl Drop for EffectHandle {
    fn drop(&mut self) {
        if let Some(slot) = &self.cleanup {
            // Only the last clone owns the disposal obligation.
            if Arc::strong_count(slot) == 1 && slot.lock().unwrap().is_some() {
                tracing::warn!("EffectHandle dropped without disposal");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn dispose_async_runs_sync_cleanup_once() {
        let ran = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&ran);
        let handle = EffectHandle::new(move || r.store(true, Ordering::SeqCst));
        handle.dispose_async().await;
        assert!(ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn dispose_async_runs_async_cleanup() {
        let ran = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&ran);
        let handle = EffectHandle::new_async(move || {
            let r = Arc::clone(&r);
            Box::pin(async move {
                tokio::task::yield_now().await;
                r.store(true, Ordering::SeqCst);
            })
        });
        handle.dispose_async().await;
        assert!(ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn none_handle_dispose_is_noop() {
        EffectHandle::none().dispose_async().await;
    }

    #[tokio::test]
    async fn into_cleanup_and_has_cleanup() {
        let handle = EffectHandle::new(|| {});
        assert!(handle.has_cleanup());
        assert!(handle.into_cleanup().is_some());

        let none = EffectHandle::none();
        assert!(!none.has_cleanup());
        assert!(none.into_cleanup().is_none());
    }

    #[test]
    fn forget_clears_cleanup() {
        let handle = EffectHandle::new(|| {});
        handle.forget();
    }

    #[tokio::test]
    async fn cloned_handle_disposes_cleanup_once() {
        let ran = Arc::new(AtomicBool::new(false));
        use std::sync::atomic::{AtomicUsize, Ordering};
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);
        let handle = EffectHandle::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            let _ = &ran;
        });
        let clone = handle.clone();
        handle.dispose_async().await;
        clone.dispose_async().await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
