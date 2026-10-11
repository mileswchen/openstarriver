//! In-memory agent registry implementing the kernel `AgentRegistry` seam.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cordis_rslite::{BoxFuture, Context, EffectHandle, TypedService};
use starriver_kernel::agent::{
    Agent, AgentCreated, AgentDisposed, AgentError, AgentFactory, AgentHandle, AgentRegistry,
    CreateAgentOptions, ResumeAgentOptions,
};
use starriver_kernel::session::SessionId;

/// One live registry entry with its runtime owner relation.
struct Entry {
    agent: Arc<dyn Agent>,
    owner: Option<Arc<dyn Agent>>,
    announced: AtomicBool,
}

/// In-memory agent registry: live entries plus one factory slot.
pub struct InMemoryAgentRegistry {
    entries: Arc<Mutex<HashMap<SessionId, Entry>>>,
    factory: Arc<Mutex<Option<Arc<dyn AgentFactory>>>>,
}

impl InMemoryAgentRegistry {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            factory: Arc::new(Mutex::new(None)),
        }
    }
}

impl Default for InMemoryAgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TypedService for InMemoryAgentRegistry {
    const NAME: &'static str = "agents";
}

impl AgentRegistry for InMemoryAgentRegistry {
    fn register(&self, agent: Arc<dyn Agent>) -> Result<EffectHandle, AgentError> {
        let id = agent.id().clone();
        let bus = agent.ctx().event_bus().cloned();
        {
            let mut entries = self.entries.lock().expect("agent registry poisoned");
            if entries.contains_key(&id) {
                return Err(AgentError::AlreadyExists(id));
            }
            entries.insert(
                id.clone(),
                Entry {
                    agent,
                    owner: None,
                    announced: AtomicBool::new(false),
                },
            );
        }
        let inner = Arc::clone(&self.entries);
        Ok(EffectHandle::new(move || {
            let removed = inner.lock().expect("agent registry poisoned").remove(&id);
            if let Some(entry) = removed {
                if entry.announced.load(Ordering::SeqCst) {
                    if let Some(bus) = &bus {
                        let disposed_agent = entry.agent.clone();
                        bus.emit_scoped(
                            disposed_agent.ctx(),
                            Arc::new(AgentDisposed {
                                agent: disposed_agent.clone(),
                            }),
                        );
                    }
                }
            }
        }))
    }

    fn enter(
        &self,
        agent: Arc<dyn Agent>,
        owner: Option<Arc<dyn Agent>>,
    ) -> Result<EffectHandle, AgentError> {
        if agent.session().id != *agent.id() {
            return Err(AgentError::Other(
                "agent id must match its session id".into(),
            ));
        }
        let id = agent.id().clone();
        let bus = agent.ctx().event_bus().cloned();
        {
            let mut entries = self.entries.lock().expect("agent registry poisoned");
            if entries.contains_key(&id) {
                return Err(AgentError::AlreadyExists(id));
            }
            entries.insert(
                id.clone(),
                Entry {
                    agent,
                    owner,
                    announced: AtomicBool::new(false),
                },
            );
        }
        let inner = Arc::clone(&self.entries);
        Ok(EffectHandle::new(move || {
            let removed = inner.lock().expect("agent registry poisoned").remove(&id);
            if let Some(entry) = removed {
                if entry.announced.load(Ordering::SeqCst) {
                    if let Some(bus) = &bus {
                        let disposed_agent = entry.agent.clone();
                        bus.emit_scoped(
                            disposed_agent.ctx(),
                            Arc::new(AgentDisposed {
                                agent: disposed_agent.clone(),
                            }),
                        );
                    }
                }
            }
        }))
    }

    fn announce(&self, agent: &Arc<dyn Agent>) -> Result<(), AgentError> {
        let bus = agent
            .ctx()
            .event_bus()
            .ok_or_else(|| AgentError::Other("agent context has no event bus".into()))?;
        {
            let mut entries = self.entries.lock().expect("agent registry poisoned");
            let Some(entry) = entries.get_mut(agent.id()) else {
                return Err(AgentError::NotFound(agent.id().clone()));
            };
            if entry.announced.swap(true, Ordering::SeqCst) {
                return Err(AgentError::Other("agent already announced".into()));
            }
        }
        bus.emit_scoped(
            agent.ctx(),
            Arc::new(AgentCreated {
                agent: Arc::clone(agent),
            }),
        );
        Ok(())
    }

    fn get(&self, id: &SessionId) -> Option<Arc<dyn Agent>> {
        self.entries
            .lock()
            .expect("agent registry poisoned")
            .get(id)
            .map(|entry| Arc::clone(&entry.agent))
    }

    fn list(&self) -> Vec<Arc<dyn Agent>> {
        self.entries
            .lock()
            .expect("agent registry poisoned")
            .values()
            .map(|entry| Arc::clone(&entry.agent))
            .collect()
    }

    fn roots(&self) -> Vec<Arc<dyn Agent>> {
        self.entries
            .lock()
            .expect("agent registry poisoned")
            .values()
            .filter(|entry| entry.owner.is_none())
            .map(|entry| Arc::clone(&entry.agent))
            .collect()
    }

    fn set_factory(&self, factory: Arc<dyn AgentFactory>) -> Result<EffectHandle, AgentError> {
        {
            let mut slot = self.factory.lock().expect("agent factory poisoned");
            if slot.is_some() {
                return Err(AgentError::Other("agent factory already registered".into()));
            }
            *slot = Some(factory);
        }
        let inner = Arc::clone(&self.factory);
        Ok(EffectHandle::new(move || {
            *inner.lock().expect("agent factory poisoned") = None;
        }))
    }

    fn create(&self, options: CreateAgentOptions) -> BoxFuture<Result<AgentHandle, AgentError>> {
        let factory = self.factory.lock().expect("agent factory poisoned").clone();
        let Some(factory) = factory else {
            return Box::pin(async { Err(AgentError::NoFactory) });
        };
        factory.create(Context::new(), options)
    }

    fn resume(&self, options: ResumeAgentOptions) -> BoxFuture<Result<AgentHandle, AgentError>> {
        let factory = self.factory.lock().expect("agent factory poisoned").clone();
        let Some(factory) = factory else {
            return Box::pin(async { Err(AgentError::NoFactory) });
        };
        factory.resume(Context::new(), options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_rslite::EventBus;
    use starriver_kernel::agent::{AgentOptions, AgentStatus};
    use starriver_kernel::llm::Message;
    use starriver_kernel::session::Session;

    struct StubAgent {
        id: SessionId,
        options: AgentOptions,
        session: Session,
        ctx: Context,
    }

    impl Agent for StubAgent {
        fn id(&self) -> &SessionId {
            &self.id
        }
        fn options(&self) -> &AgentOptions {
            &self.options
        }
        fn session(&self) -> &Session {
            &self.session
        }
        fn status(&self) -> AgentStatus {
            AgentStatus::Idle
        }
        fn ctx(&self) -> &Context {
            &self.ctx
        }
        fn followup(&self, _message: Message) {}
        fn steer(&self, _message: Message) {}
        fn inject(&self, _message: Message) {}
        fn send(
            &self,
            _message: Message,
            _target: starriver_kernel::agent::InboxTarget,
            _wakeup: bool,
        ) {
        }
        fn cancel(&self, _cause: starriver_kernel::agent::AgentCancelCause, _keep_inbox: bool) -> bool { false }
        fn when_idle(&self) -> BoxFuture<()> {
            Box::pin(async {})
        }
    }

    fn stub_agent(id: &str) -> Arc<dyn Agent> {
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let session = Session::new(SessionId(id.into()), bus);
        Arc::new(StubAgent {
            id: SessionId(id.into()),
            options: AgentOptions::default(),
            session,
            ctx,
        })
    }

    #[test]
    fn register_get_and_enter_collision() {
        let registry = InMemoryAgentRegistry::new();
        let agent = stub_agent("a1");

        let _handle = registry.register(Arc::clone(&agent)).unwrap();
        assert!(registry.get(&SessionId("a1".into())).is_some());

        // enter is the authoritative staged path: duplicate id rejects.
        let second = stub_agent("a1");
        assert!(matches!(
            registry.enter(second, None),
            Err(AgentError::AlreadyExists(_))
        ));
    }

    #[tokio::test]
    async fn announce_emits_agent_created() {
        use starriver_kernel::agent::AgentCreated;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let registry = InMemoryAgentRegistry::new();
        let agent = stub_agent("a2");
        let bus = agent.ctx().event_bus().unwrap().clone();

        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let _listener = bus.on::<AgentCreated, _>(move |_| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(cordis_rslite::Value::new(()))
            })
        });

        let _enter = registry.enter(Arc::clone(&agent), None).unwrap();
        registry.announce(&agent).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.roots().len(), 1);
    }

    #[test]
    fn create_without_factory_rejects() {
        let registry = InMemoryAgentRegistry::new();
        let result = registry.create(CreateAgentOptions {
            session_id: SessionId("a3".into()),
            seed: Vec::new(),
            cwd: None,
            agent_options: AgentOptions::default(),
            agent_preset: None,
        });
        let error = match futures::executor::block_on(result) {
            Ok(_) => panic!("expected NoFactory"),
            Err(error) => error,
        };
        assert!(matches!(error, AgentError::NoFactory));
    }

    #[test]
    fn set_factory_and_clear() {
        let registry = InMemoryAgentRegistry::new();
        struct StubFactory;
        impl AgentFactory for StubFactory {
            fn create(
                &self,
                _owner: Context,
                _options: CreateAgentOptions,
            ) -> BoxFuture<Result<AgentHandle, AgentError>> {
                Box::pin(async { Err(AgentError::Other("unused".into())) })
            }
            fn resume(
                &self,
                _owner: Context,
                _options: ResumeAgentOptions,
            ) -> BoxFuture<Result<AgentHandle, AgentError>> {
                Box::pin(async { Err(AgentError::NoPersistence) })
            }
        }
        let _slot = registry.set_factory(Arc::new(StubFactory)).unwrap();
    }

    #[test]
    fn enter_rejects_identity_mismatch() {
        let registry = InMemoryAgentRegistry::new();
        let bus = EventBus::new();
        let ctx = Context::new().with_event_bus(&bus);
        let session = Session::new(SessionId("other".into()), bus);
        let agent = Arc::new(StubAgent {
            id: SessionId("a1".into()),
            options: AgentOptions::default(),
            session,
            ctx,
        }) as Arc<dyn Agent>;

        let error = match registry.enter(agent, None) {
            Ok(_) => panic!("expected identity mismatch to fail"),
            Err(error) => error,
        };
        assert!(matches!(error, AgentError::Other(_)));
    }

    #[test]
    fn announce_twice_rejected() {
        let registry = InMemoryAgentRegistry::new();
        let agent = stub_agent("a6");
        let _enter = registry.enter(Arc::clone(&agent), None).unwrap();
        registry.announce(&agent).unwrap();
        let error = registry.announce(&agent).unwrap_err();
        assert!(matches!(error, AgentError::Other(_)));
    }

    #[tokio::test]
    async fn dispose_emits_agent_disposed_when_announced() {
        use starriver_kernel::agent::AgentDisposed;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let registry = InMemoryAgentRegistry::new();
        let agent = stub_agent("a7");
        let bus = agent.ctx().event_bus().unwrap().clone();
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        let _listener = bus.on::<AgentDisposed, _>(move |_| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(cordis_rslite::Value::new(()))
            })
        });

        let _enter = registry.enter(Arc::clone(&agent), None).unwrap();
        registry.announce(&agent).unwrap();
        _enter.dispose_async().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert!(registry.get(&SessionId("a7".into())).is_none());
    }
}
