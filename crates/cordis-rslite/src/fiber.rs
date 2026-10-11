use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::FutureExt;

use crate::effect::EffectHandle;
type Cleanup = Box<dyn FnOnce() -> crate::BoxFuture<()> + Send>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiberState {
    Pending = 0,
    Loading = 1,
    Active = 2,
    Failed = 3,
    Disposing = 4,
    Disposed = 5,
}

impl FiberState {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Pending,
            1 => Self::Loading,
            2 => Self::Active,
            3 => Self::Failed,
            4 => Self::Disposing,
            _ => Self::Disposed,
        }
    }
}

pub struct Fiber {
    state: AtomicU8,
    cleanups: Mutex<Vec<Cleanup>>,
}

impl Fiber {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(FiberState::Pending as u8),
            cleanups: Mutex::new(Vec::new()),
        })
    }

    pub fn state(&self) -> FiberState {
        FiberState::from_u8(self.state.load(Ordering::SeqCst))
    }

    pub fn set_loading(&self) -> bool {
        loop {
            let s = self.state.load(Ordering::SeqCst);
            match s {
                x if x == FiberState::Pending as u8 || x == FiberState::Failed as u8 => {
                    if self
                        .state
                        .compare_exchange(
                            x,
                            FiberState::Loading as u8,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
                _ => return false,
            }
        }
    }

    pub fn set_active(&self) -> bool {
        self.state
            .compare_exchange(
                FiberState::Loading as u8,
                FiberState::Active as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    pub fn set_failed(&self) -> bool {
        self.state
            .compare_exchange(
                FiberState::Loading as u8,
                FiberState::Failed as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    pub fn set_pending(&self) -> bool {
        loop {
            let s = self.state.load(Ordering::SeqCst);
            match s {
                x if x == FiberState::Loading as u8
                    || x == FiberState::Active as u8
                    || x == FiberState::Failed as u8 =>
                {
                    if self
                        .state
                        .compare_exchange(
                            x,
                            FiberState::Pending as u8,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
                _ => return false,
            }
        }
    }

    pub fn adopt(&self, cleanup: Cleanup) -> Result<(), &'static str> {
        let mut cleanups = self.cleanups.lock().expect("cleanup list poisoned");
        if self.state.load(Ordering::SeqCst) >= FiberState::Disposing as u8 {
            drop(cleanups);
            tracing::warn!("adopt rejected on disposing fiber; cleanup skipped");
            return Err("fiber is disposing or disposed");
        }
        cleanups.push(cleanup);
        Ok(())
    }

    pub fn adopt_effect(&self, effect: EffectHandle) -> Result<(), &'static str> {
        match effect.into_cleanup() {
            Some(cleanup) => self.adopt(cleanup),
            None => Ok(()),
        }
    }

    async fn run_cleanups(cleanups: Vec<Cleanup>) {
        for cleanup in cleanups.into_iter().rev() {
            let _ = std::panic::AssertUnwindSafe(async move { cleanup().await })
                .catch_unwind()
                .await;
        }
    }

    pub(crate) async fn clear_effects(&self) {
        let cleanups = {
            let mut v = self.cleanups.lock().expect("cleanup list poisoned");
            std::mem::take(&mut *v)
        };
        Self::run_cleanups(cleanups).await;
    }

    pub async fn dispose(self: &Arc<Self>) {
        loop {
            let s = self.state.load(Ordering::SeqCst);
            match s {
                x if x == FiberState::Pending as u8
                    || x == FiberState::Loading as u8
                    || x == FiberState::Active as u8
                    || x == FiberState::Failed as u8 =>
                {
                    if self
                        .state
                        .compare_exchange(
                            x,
                            FiberState::Disposing as u8,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        self.clear_effects().await;
                        self.state
                            .store(FiberState::Disposed as u8, Ordering::SeqCst);
                        return;
                    }
                }
                _ => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn dispose_runs_cleanups_in_reverse_order() {
        let fiber = Fiber::new();
        let log = Arc::new(Mutex::new(Vec::new()));
        let l1 = Arc::clone(&log);
        let l2 = Arc::clone(&log);
        fiber
            .adopt(Box::new(move || {
                let l = Arc::clone(&l1);
                Box::pin(async move { l.lock().unwrap().push(1) })
            }))
            .unwrap();
        fiber
            .adopt(Box::new(move || {
                let l = Arc::clone(&l2);
                Box::pin(async move { l.lock().unwrap().push(2) })
            }))
            .unwrap();
        fiber.dispose().await;
        assert_eq!(*log.lock().unwrap(), vec![2, 1]);
        assert_eq!(fiber.state(), FiberState::Disposed);
    }

    #[tokio::test]
    async fn adopt_rejected_after_dispose() {
        let fiber = Fiber::new();
        fiber.dispose().await;
        assert!(fiber.adopt(Box::new(|| Box::pin(async {}))).is_err());
    }

    #[tokio::test]
    async fn double_dispose_runs_cleanups_once() {
        let fiber = Fiber::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        fiber
            .adopt(Box::new(move || {
                let c = Arc::clone(&c);
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                })
            }))
            .unwrap();
        fiber.dispose().await;
        fiber.dispose().await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lifecycle_transitions() {
        let fiber = Fiber::new();
        assert_eq!(fiber.state(), FiberState::Pending);
        fiber.set_loading();
        assert_eq!(fiber.state(), FiberState::Loading);
        fiber.set_active();
        assert_eq!(fiber.state(), FiberState::Active);
        fiber.set_pending();
        assert_eq!(fiber.state(), FiberState::Pending);
        fiber.set_loading();
        fiber.set_failed();
        assert_eq!(fiber.state(), FiberState::Failed);
    }

    #[test]
    fn invalid_transition_is_rejected() {
        let fiber = Fiber::new();
        assert!(!fiber.set_active());
        assert!(fiber.set_loading());
        assert!(!fiber.set_loading());
    }

    #[tokio::test]
    async fn dispose_continues_after_cleanup_panic() {
        let fiber = Fiber::new();
        let ran = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&ran);
        fiber
            .adopt(Box::new(|| Box::pin(async { panic!("boom") })))
            .unwrap();
        fiber
            .adopt(Box::new(move || {
                let r = Arc::clone(&r);
                Box::pin(async move {
                    r.store(true, Ordering::SeqCst);
                })
            }))
            .unwrap();
        fiber.dispose().await;
        assert_eq!(fiber.state(), FiberState::Disposed);
        assert!(ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn clear_effects_runs_and_empties_cleanups() {
        let fiber = Fiber::new();
        let ran = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&ran);
        fiber
            .adopt(Box::new(move || {
                let r = Arc::clone(&r);
                Box::pin(async move {
                    r.store(true, Ordering::SeqCst);
                })
            }))
            .unwrap();
        fiber.clear_effects().await;
        assert!(ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn set_loading_from_failed_retries() {
        let fiber = Fiber::new();
        fiber.set_loading();
        fiber.set_failed();
        assert!(fiber.set_loading());
    }

    #[tokio::test]
    async fn set_pending_from_failed() {
        let fiber = Fiber::new();
        fiber.set_loading();
        fiber.set_failed();
        assert!(fiber.set_pending());
    }

    #[tokio::test]
    async fn dispose_from_failed_works() {
        let fiber = Fiber::new();
        fiber.set_loading();
        fiber.set_failed();
        fiber.dispose().await;
        assert_eq!(fiber.state(), FiberState::Disposed);
    }

    #[tokio::test]
    async fn dispose_from_loading_works() {
        let fiber = Fiber::new();
        fiber.set_loading();
        fiber.dispose().await;
        assert_eq!(fiber.state(), FiberState::Disposed);
    }

    #[test]
    fn full_transition_matrix() {
        fn pending() -> Arc<Fiber> {
            Fiber::new()
        }
        fn loading() -> Arc<Fiber> {
            let fiber = Fiber::new();
            fiber.set_loading();
            fiber
        }
        fn active() -> Arc<Fiber> {
            let fiber = loading();
            fiber.set_active();
            fiber
        }
        fn failed() -> Arc<Fiber> {
            let fiber = loading();
            fiber.set_failed();
            fiber
        }

        assert!(pending().set_loading());
        assert!(failed().set_loading());
        assert!(!loading().set_loading());
        assert!(!active().set_loading());

        assert!(!pending().set_active());
        assert!(loading().set_active());
        assert!(!active().set_active());
        assert!(!failed().set_active());

        assert!(!pending().set_failed());
        assert!(loading().set_failed());
        assert!(!active().set_failed());
        assert!(!failed().set_failed());

        assert!(!pending().set_pending());
        assert!(loading().set_pending());
        assert!(active().set_pending());
        assert!(failed().set_pending());
    }

    #[tokio::test]
    async fn dispose_runs_async_cleanups_in_reverse() {
        let fiber = Fiber::new();
        let log = Arc::new(Mutex::new(Vec::new()));
        let l1 = Arc::clone(&log);
        let l2 = Arc::clone(&log);
        fiber
            .adopt(Box::new(move || {
                let l = Arc::clone(&l1);
                Box::pin(async move { l.lock().unwrap().push(1) })
            }))
            .unwrap();
        fiber
            .adopt(Box::new(move || {
                let l = Arc::clone(&l2);
                Box::pin(async move { l.lock().unwrap().push(2) })
            }))
            .unwrap();
        fiber.dispose().await;
        assert_eq!(*log.lock().unwrap(), vec![2, 1]);
    }

    #[tokio::test]
    async fn adopt_effect_none_is_noop() {
        let fiber = Fiber::new();
        assert!(fiber.adopt_effect(EffectHandle::none()).is_ok());
    }
}
