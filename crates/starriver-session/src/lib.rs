//! Default in-memory session backend.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use cordis_rslite::{Context, EventBus, Inject, Plugin, TypedService};
use starriver_kernel::session::{
    EnterHandle, PrepareOptions, Session, SessionCreated, SessionDisposed, SessionError, SessionId,
    SessionPreparation, SessionStore,
};

struct Entry {
    session: Session,
    announced: AtomicBool,
}

/// Default in-memory session backend implementing the three-stage
/// prepare/enter/announce publication contract with `session/created` veto.
pub struct InMemorySessionStore {
    shared: Arc<StoreShared>,
    ctx: Context,
}

struct StoreShared {
    sessions: Mutex<HashMap<SessionId, Entry>>,
    bus: EventBus,
}

impl InMemorySessionStore {
    pub fn new(bus: EventBus, ctx: Context) -> Self {
        Self {
            shared: Arc::new(StoreShared {
                sessions: Mutex::new(HashMap::new()),
                bus,
            }),
            ctx,
        }
    }

    fn detach(&self, id: &SessionId) {
        let removed = self
            .shared
            .sessions
            .lock()
            .expect("session map poisoned")
            .remove(id);
        if let Some(entry) = removed {
            if entry.announced.load(std::sync::atomic::Ordering::SeqCst) {
                self.shared.bus.emit(Arc::new(SessionDisposed {
                    session: entry.session,
                }));
            }
        }
    }
}

impl TypedService for InMemorySessionStore {
    const NAME: &'static str = <InMemorySessionStore as SessionStore>::NAME;
}

impl SessionStore for InMemorySessionStore {
    fn prepare(
        &self,
        id: SessionId,
        options: PrepareOptions,
    ) -> Result<SessionPreparation, SessionError> {
        let mut session = Session::new(id, self.shared.bus.clone()).with_target(self.ctx.clone());
        if let Some(cwd) = options.cwd {
            let mut header = session.header.clone();
            header.cwd = Some(cwd);
            session = session.with_header(header)?;
        }
        if let Some(agent_preset) = options.agent_preset {
            let mut header = session.header.clone();
            header.agent_preset = Some(agent_preset);
            session = session.with_header(header)?;
        }
        if !options.seed.is_empty() {
            session = session.with_seed(options.seed)?;
        }
        Ok(SessionPreparation::new(session))
    }

    fn enter(&self, session: Session) -> Result<EnterHandle, SessionError> {
        let id = session.id.clone();
        {
            let mut sessions = self.shared.sessions.lock().expect("session map poisoned");
            if sessions.contains_key(&id) {
                return Err(SessionError::AlreadyExists);
            }
            sessions.insert(
                id.clone(),
                Entry {
                    session,
                    announced: AtomicBool::new(false),
                },
            );
        }
        let shared = Arc::clone(&self.shared);
        Ok(EnterHandle::new(Box::new(move || {
            let removed = shared
                .sessions
                .lock()
                .expect("session map poisoned")
                .remove(&id);
            if let Some(entry) = removed {
                if entry.announced.load(std::sync::atomic::Ordering::SeqCst) {
                    shared.bus.emit(Arc::new(SessionDisposed {
                        session: entry.session,
                    }));
                }
            }
        })))
    }

    fn announce(&self, session: &Session) -> Result<(), SessionError> {
        {
            let mut sessions = self.shared.sessions.lock().expect("session map poisoned");
            let Some(entry) = sessions.get_mut(&session.id) else {
                return Err(SessionError::NotFound);
            };
            if entry
                .announced
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(SessionError::Dispatch("session already announced".into()));
            }
        }

        let event = Arc::new(SessionCreated {
            session: session.clone(),
        });
        let results = self.shared.bus.emit_sync_scoped(&self.ctx, event);
        if results.iter().any(|result| result.is_err()) {
            self.detach(&session.id);
            return Err(SessionError::Dispatch(
                results
                    .into_iter()
                    .filter_map(Result::err)
                    .map(|error| error.to_string())
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        Ok(())
    }

    fn get(&self, id: &SessionId) -> Option<Session> {
        self.shared
            .sessions
            .lock()
            .expect("session map poisoned")
            .get(id)
            .map(|entry| entry.session.clone())
    }

    fn remove(&self, id: &SessionId) -> Option<Session> {
        let removed = self
            .shared
            .sessions
            .lock()
            .expect("session map poisoned")
            .remove(id);
        if let Some(entry) = &removed {
            if entry.announced.load(std::sync::atomic::Ordering::SeqCst) {
                self.shared.bus.emit(Arc::new(SessionDisposed {
                    session: entry.session.clone(),
                }));
            }
        }
        removed.map(|entry| entry.session)
    }
}

pub fn in_memory_session_plugin() -> impl Plugin {
    struct InMemorySessionPlugin;

    impl Plugin for InMemorySessionPlugin {
        fn name(&self) -> &'static str {
            "in-memory-session"
        }

        fn inject(&self) -> Inject {
            Inject::none()
        }

        fn apply(
            &self,
            ctx: &Context,
            bus: &EventBus,
        ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<cordis_rslite::EffectHandle>>> {
            let ctx = ctx.clone();
            let bus = bus.clone();
            Box::pin(async move {
                let effect = ctx.provide::<InMemorySessionStore>(Arc::new(
                    InMemorySessionStore::new(bus, ctx.clone()),
                ))?;
                Ok(vec![effect])
            })
        }
    }

    InMemorySessionPlugin
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_rslite::{Context, EventBus, EventError, PluginRegistry, Value};
    use starriver_kernel::json::JsonValue;

    #[test]
    fn session_append_events_and_flush() {
        let bus = EventBus::new();
        let session = Session::new(SessionId("s1".into()), bus.clone());
        let event = session.append("turn/start", JsonValue::UInt(1)).unwrap();
        assert_eq!(event.seq, 0);
        assert_eq!(session.events().len(), 1);
    }

    #[test]
    fn store_create_get_remove() {
        let bus = EventBus::new();
        let ctx = Context::new();
        let store = InMemorySessionStore::new(bus, ctx);
        let id = SessionId("s1".into());
        let session = store.create(id.clone(), PrepareOptions::default()).unwrap();
        assert!(store.get(&id).is_some());
        let removed = store.remove(&id).unwrap();
        assert_eq!(removed.id, session.id);
        assert!(store.get(&id).is_none());
    }

    #[test]
    fn prepare_stamps_agent_preset_into_the_header() {
        let bus = EventBus::new();
        let ctx = Context::new();
        let store = InMemorySessionStore::new(bus, ctx);
        let id = SessionId("s-pres".into());
        let session = store
            .create(
                id,
                PrepareOptions {
                    cwd: None,
                    seed: vec![],
                    agent_preset: Some("coder".into()),
                },
            )
            .unwrap();
        assert_eq!(session.header.agent_preset.as_deref(), Some("coder"));
        // Absent → header default (None) stays untouched.
        let session = store
            .create(
                SessionId("s-pres2".into()),
                PrepareOptions {
                    cwd: None,
                    seed: vec![],
                    agent_preset: None,
                },
            )
            .unwrap();
        assert!(session.header.agent_preset.is_none());
    }

    #[tokio::test]
    async fn create_publishes_and_veto_rolls_back() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let bus = EventBus::new();
        let ctx = Context::new();
        let store = InMemorySessionStore::new(bus.clone(), ctx);

        let disposed = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&disposed);
        let _disposed_listener = bus.on::<SessionDisposed, _>(move |_| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Value::new(()))
            })
        });

        let _veto =
            bus.on_sync::<SessionCreated, _>(|_| Err(EventError::Listener("veto creation".into())));

        let id = SessionId("s1".into());
        let error = match store.create(id.clone(), PrepareOptions::default()) {
            Ok(_) => panic!("expected veto to fail creation"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("dispatch"));
        assert!(store.get(&id).is_none());
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(disposed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn enter_announce_dispose_pairs_disposal() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let bus = EventBus::new();
        let ctx = Context::new();
        let store = InMemorySessionStore::new(bus.clone(), ctx);

        let disposed = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&disposed);
        let _disposed_listener = bus.on::<SessionDisposed, _>(move |_| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Value::new(()))
            })
        });

        let id = SessionId("s2".into());
        let preparation = store
            .prepare(id.clone(), PrepareOptions::default())
            .unwrap();
        let session = preparation.session.clone();
        let handle = store.enter(session.clone()).unwrap();
        store.announce(&session).unwrap();
        assert!(store.get(&id).is_some());

        handle.dispose();
        assert!(store.get(&id).is_none());
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(disposed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn announce_twice_and_duplicate_enter_are_rejected() {
        let bus = EventBus::new();
        let ctx = Context::new();
        let store = InMemorySessionStore::new(bus.clone(), ctx);

        let id = SessionId("s3".into());
        let preparation = store
            .prepare(id.clone(), PrepareOptions::default())
            .unwrap();
        let session = preparation.session.clone();
        let _handle = store.enter(session.clone()).unwrap();
        store.announce(&session).unwrap();

        let second_preparation = store
            .prepare(id.clone(), PrepareOptions::default())
            .unwrap();
        assert!(matches!(
            store.enter(second_preparation.session.clone()),
            Err(SessionError::AlreadyExists)
        ));
        assert!(matches!(
            store.announce(&session),
            Err(SessionError::Dispatch(_))
        ));
    }

    #[tokio::test]
    async fn plugin_provides_session_store() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(in_memory_session_plugin());
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(ctx.require::<InMemorySessionStore>().is_ok());
        assert_eq!(handles.len(), 1);
    }
}

/// Legacy sqlite-backed session store (`starriver-core::session`), moved
/// here during the core slim-down. Superseded long-term by
/// `starriver-session-persistence`; kept for the host and edge runtimes.
pub mod compaction;
pub mod legacy;
