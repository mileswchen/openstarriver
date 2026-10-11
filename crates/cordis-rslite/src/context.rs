use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::effect::EffectHandle;
use crate::events::{Event, EventBus, EventError, EventOptions, Next, WaterfallEvent};
use crate::fiber::{Fiber, FiberState};
use crate::plugin::{Plugin, PluginHandle, PluginKey, RegistryState};
use crate::service::{Service, TypedService};
use crate::value::{Config, Value, ValueCastError};

pub const KERNEL_PREFIX: &str = "kernel:";
pub const SEAM_PREFIX: &str = "seam:";

pub fn is_public_seam(name: &str) -> bool {
    name.starts_with(SEAM_PREFIX)
}

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("service `{0}` is not registered")]
    Missing(String),
    #[error("service `{0}` has already been provided in this scope")]
    Duplicate(String),
    #[error("service `{name}` has type `{actual}`, expected `{expected}`")]
    TypeMismatch {
        name: String,
        expected: &'static str,
        actual: &'static str,
    },
    #[error("event dispatch failed: {0}")]
    Event(#[from] EventError),
    #[error("value cast failed: {0}")]
    Cast(#[from] ValueCastError),
    #[error("provider fiber is disposing; registration refused: {0}")]
    ProviderDisposing(String),
}

struct Entry {
    value: Value,
    type_name: &'static str,
    generation: u64,
    provider: Weak<Fiber>,
    check: Option<AvailabilityCheck>,
}

/// Concrete carrier for `Arc<dyn Service>` values stored in a [`Value`].
///
/// `Value`'s `Any` downcast only reaches concrete types, so dyn services are
/// boxed in this wrapper by [`Context::provide_dyn`] and unwrapped by
/// [`Context::require_dyn`].
struct DynServiceValue(Arc<dyn Service>);

/// Opaque service-scope label used by [`Context::isolate_with`].
///
/// Labels returned by [`Context::new_isolation`] are preferred because they
/// cannot accidentally collide with labels allocated by another context that
/// happens to use the same raw number. Reusing one label across separate
/// context branches joins them to the same service implementation slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Isolation(pub(crate) u64);

impl Isolation {
    /// Create a user-defined label from a raw number.
    ///
    /// Prefer [`Context::new_isolation`] for new labels. `from_raw` exists for
    /// persistence/diagnostics round-trips and for test fixtures that need a
    /// stable label.
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }

    /// Expose the numeric label for persistence or diagnostics.
    pub const fn as_raw(self) -> u64 {
        self.0
    }
}

type Subscriber = Arc<dyn Fn(&str) + Send + Sync>;
type AvailabilityCheck = Arc<dyn Fn(&Value) -> bool + Send + Sync>;

pub type Spawner = Arc<
    dyn Fn(Arc<dyn Plugin>, Arc<Fiber>, Config) -> crate::BoxFuture<anyhow::Result<PluginHandle>>
        + Send
        + Sync,
>;

/// Predicate attached to a [`Context`] that decides which listener owner
/// contexts are eligible for events dispatched with that context as the
/// target.
///
/// This mirrors Cordis' `Context.filter`: the filter receives the context a
/// listener was registered in, and returns whether that listener should run.
/// Untagged/global listeners are accepted before the filter is consulted.
pub type ContextFilter = Arc<dyn Fn(&Context) -> bool + Send + Sync + 'static>;

/// Mutable service storage shared by every context cloned from the same root.
#[derive(Default)]
struct ServiceState {
    /// Lazily-allocated default scope for each service name.
    default_scopes: HashMap<String, Isolation>,
    /// One implementation slot per [`Isolation`].
    implementations: HashMap<Isolation, Entry>,
}

struct Inner {
    state: Mutex<ServiceState>,
    next_generation: AtomicU64,
    next_scope: AtomicU64,
    provided_by: Mutex<HashMap<usize, HashSet<String>>>,
    subscribers: Mutex<Vec<(u64, Subscriber)>>,
    next_subscriber_id: AtomicU64,
    registry: Mutex<Option<Weak<RegistryState>>>,
}

impl Inner {
    fn notify(&self, name: &str) {
        let subscribers = {
            let subs = self.subscribers.lock().expect("subscriber list poisoned");
            subs.iter()
                .map(|(_, cb)| Arc::clone(cb))
                .collect::<Vec<_>>()
        };
        for cb in subscribers {
            cb(name);
        }
    }
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            state: Mutex::new(ServiceState::default()),
            next_generation: AtomicU64::new(0),
            next_scope: AtomicU64::new(0),
            provided_by: Mutex::new(HashMap::new()),
            subscribers: Mutex::new(Vec::new()),
            next_subscriber_id: AtomicU64::new(0),
            registry: Mutex::new(None),
        }
    }
}

impl Inner {
    fn next_isolation(&self) -> Isolation {
        Isolation(self.next_scope.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// Resolve the scope label for `name`, preferring a context override over
    /// the lazily-allocated default.
    fn scope_for(&self, ctx: &Context, name: &str) -> Option<Isolation> {
        if let Some(scope) = ctx.isolates.get(name) {
            return Some(*scope);
        }
        let state = self.state.lock().expect("service state poisoned");
        state.default_scopes.get(name).copied()
    }

    /// Resolve `name` to a scope label, allocating a default label when the
    /// name has never been provided before.
    fn ensure_scope(&self, ctx: &Context, name: &str) -> Isolation {
        if let Some(scope) = ctx.isolates.get(name) {
            return *scope;
        }
        let mut state = self.state.lock().expect("service state poisoned");
        if let Some(scope) = state.default_scopes.get(name) {
            return *scope;
        }
        let scope = self.next_isolation();
        state.default_scopes.insert(name.to_owned(), scope);
        scope
    }
}

#[derive(Clone)]
pub struct Context {
    inner: Arc<Inner>,
    provider: Weak<Fiber>,
    spawner: Option<Spawner>,
    bus: Option<EventBus>,
    intercepts: Arc<HashMap<String, Vec<Value>>>,
    filter: Option<ContextFilter>,
    isolates: Arc<HashMap<String, Isolation>>,
    /// Opaque scope tag propagated to every child context. Agent scopes use it
    /// to route scoped event dispatch; the kernel never interprets the value.
    tag: Option<Arc<str>>,
    /// Effects registered on this context branch, disposed together by
    /// [`Context::dispose_effects`]. `tagged` starts a fresh branch; other
    /// builders share the same branch.
    effects: Arc<Mutex<Vec<EffectHandle>>>,
}

impl Context {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner::default()),
            provider: Weak::new(),
            spawner: None,
            bus: None,
            intercepts: Arc::new(HashMap::new()),
            filter: None,
            isolates: Arc::new(HashMap::new()),
            tag: None,
            effects: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Context {
    pub fn scoped(&self, provider: Weak<Fiber>) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            provider,
            spawner: self.spawner.clone(),
            bus: self.bus.clone(),
            intercepts: self.intercepts.clone(),
            filter: self.filter.clone(),
            isolates: Arc::clone(&self.isolates),
            tag: self.tag.clone(),
            effects: self.effects.clone(),
        }
    }

    /// Allocate a globally unique isolation label.
    pub fn new_isolation(&self) -> Isolation {
        self.inner.next_isolation()
    }

    /// Create a child context with a fresh independent service scope for
    /// `name`.
    pub fn isolate(&self, name: impl Into<String>) -> Self {
        let label = self.new_isolation();
        self.isolate_with(name, label)
    }

    /// Create a child context using a supplied scope label for `name`.
    ///
    /// Reusing the label joins otherwise separate context branches to the
    /// same service implementation slot.
    pub fn isolate_with(&self, name: impl Into<String>, label: Isolation) -> Self {
        let mut isolates = (*self.isolates).clone();
        isolates.insert(name.into(), label);
        Self {
            inner: Arc::clone(&self.inner),
            provider: Weak::new(),
            spawner: None,
            bus: self.bus.clone(),
            intercepts: self.intercepts.clone(),
            filter: self.filter.clone(),
            isolates: Arc::new(isolates),
            tag: self.tag.clone(),
            effects: self.effects.clone(),
        }
    }

    /// Create a child context with several service names isolated together.
    ///
    /// All `names` share one scope label and therefore one implementation
    /// slot. When `label` is omitted a fresh label is allocated.
    pub fn isolate_many(
        &self,
        names: impl IntoIterator<Item = impl Into<String>>,
        label: Option<Isolation>,
    ) -> Self {
        let label = label.unwrap_or_else(|| self.new_isolation());
        let mut isolates = (*self.isolates).clone();
        for name in names {
            isolates.insert(name.into(), label);
        }
        Self {
            inner: Arc::clone(&self.inner),
            provider: Weak::new(),
            spawner: None,
            bus: self.bus.clone(),
            intercepts: self.intercepts.clone(),
            filter: self.filter.clone(),
            isolates: Arc::new(isolates),
            tag: self.tag.clone(),
            effects: self.effects.clone(),
        }
    }

    pub fn with_spawner(&self, spawner: Spawner) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            provider: self.provider.clone(),
            spawner: Some(spawner),
            bus: self.bus.clone(),
            intercepts: self.intercepts.clone(),
            filter: self.filter.clone(),
            isolates: Arc::clone(&self.isolates),
            tag: self.tag.clone(),
            effects: self.effects.clone(),
        }
    }

    pub fn with_event_bus(&self, bus: &EventBus) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            provider: self.provider.clone(),
            spawner: self.spawner.clone(),
            bus: Some(bus.clone()),
            intercepts: self.intercepts.clone(),
            filter: self.filter.clone(),
            isolates: Arc::clone(&self.isolates),
            tag: self.tag.clone(),
            effects: self.effects.clone(),
        }
    }

    /// Attach a weak reference to the owning plugin registry.
    ///
    /// The registry is stored on the shared [`Inner`], so every context cloned
    /// from the same root sees it. The reference is deliberately weak to avoid
    /// a cycle: the registry holds a strong [`Context`], while the context
    /// only upgrades the registry at plugin-start time.
    pub(crate) fn set_registry(&self, registry: Weak<RegistryState>) {
        *self
            .inner
            .registry
            .lock()
            .expect("registry pointer poisoned") = Some(registry);
    }

    /// Return a child context with an event-listener filter attached.
    ///
    /// The filter is consulted when this context is used as the target of an
    /// `*_scoped` event dispatch: a listener whose owning context does not pass
    /// the filter is skipped. Listeners registered with
    /// [`EventOptions::global`] or with no owner (via [`EventBus`]) still run.
    pub fn with_filter<F>(&self, filter: F) -> Self
    where
        F: Fn(&Context) -> bool + Send + Sync + 'static,
    {
        Self {
            inner: Arc::clone(&self.inner),
            provider: self.provider.clone(),
            spawner: self.spawner.clone(),
            bus: self.bus.clone(),
            intercepts: self.intercepts.clone(),
            filter: Some(Arc::new(filter)),
            isolates: Arc::clone(&self.isolates),
            tag: self.tag.clone(),
            effects: self.effects.clone(),
        }
    }

    /// Return this context's event-listener filter, if any.
    pub fn filter(&self) -> Option<&ContextFilter> {
        self.filter.as_ref()
    }

    /// Return a child context carrying an opaque scope tag. The tag
    /// propagates through every child context and is consulted by agent-scope
    /// event filters.
    pub fn tagged(&self, tag: Arc<str>) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            provider: self.provider.clone(),
            spawner: self.spawner.clone(),
            bus: self.bus.clone(),
            intercepts: self.intercepts.clone(),
            filter: self.filter.clone(),
            isolates: Arc::clone(&self.isolates),
            tag: Some(tag),
            effects: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The opaque scope tag attached to this context, if any.
    pub fn tag(&self) -> Option<&Arc<str>> {
        self.tag.as_ref()
    }

    /// The event bus attached to this context, if any.
    pub fn event_bus(&self) -> Option<&EventBus> {
        self.bus.as_ref()
    }

    /// Register a cleanup that runs when this context branch is disposed via
    /// [`Context::dispose_effects`], or — when this context is scoped to a
    /// provider fiber — when that fiber clears or disposes its effects.
    pub fn effect(&self, cleanup: impl FnOnce() + Send + 'static) -> EffectHandle {
        let handle = EffectHandle::new(cleanup);
        self.track_effect(handle.clone());
        handle
    }

    /// Track an effect registration made through this context.
    ///
    /// Ownership rule (caller-fiber semantics): when the context is scoped to
    /// a provider fiber, the registration's cleanup is adopted by that fiber,
    /// so a plugin reload (`Fiber::clear_effects`) or disposal cleans up the
    /// effects *it* registered — listeners, cleanups, and provided services
    /// alike. Contexts without a current fiber (root, isolated, or tagged
    /// scopes) keep branch-scope disposal via
    /// [`Context::dispose_effects`]. A provider being deactivated never
    /// touches a dependent's effects; dependents re-evaluate themselves
    /// through reconcile.
    fn track_effect(&self, handle: EffectHandle) {
        if let Some(fiber) = self.provider.upgrade() {
            // Only hand the cleanup over when the fiber can actually adopt
            // it: on a Disposing/Disposed fiber, adopt() both rejects AND
            // drops the closure — into_cleanup already emptied the handle's
            // slot, so the listener would leak in the bus forever with an
            // inert handle. Keep branch-scope disposal in that case.
            if (fiber.state() as u8) < FiberState::Disposing as u8 {
                if let Some(cleanup) = handle.clone().into_cleanup() {
                    if fiber.adopt(cleanup).is_ok() {
                        // Fiber-owned registration: the branch list must not
                        // accumulate inert husks across plugin hot reloads.
                        return;
                    }
                }
            }
        }
        self.effects.lock().unwrap().push(handle);
    }

    /// Run every effect registered on this context branch, in registration
    /// order, and clear the branch list.
    pub async fn dispose_effects(&self) {
        let effects = std::mem::take(&mut *self.effects.lock().unwrap());
        for handle in effects {
            handle.dispose_async().await;
        }
    }

    /// Return a child context with one additional intercept config for `name`.
    ///
    /// Intercept configs are accumulated root-most first, so a plugin started
    /// below several `intercept` calls sees them in ancestor order. The parent
    /// context is not mutated.
    pub fn intercept(&self, name: impl Into<String>, config: Value) -> Self {
        let name = name.into();
        let mut intercepts = (*self.intercepts).clone();
        intercepts.entry(name).or_default().push(config);
        Self {
            inner: Arc::clone(&self.inner),
            provider: self.provider.clone(),
            spawner: self.spawner.clone(),
            bus: self.bus.clone(),
            intercepts: Arc::new(intercepts),
            filter: self.filter.clone(),
            isolates: Arc::clone(&self.isolates),
            tag: self.tag.clone(),
            effects: self.effects.clone(),
        }
    }

    /// Typed convenience wrapper around [`Context::intercept`].
    pub fn intercept_typed<T>(&self, name: impl Into<String>, config: T) -> Self
    where
        T: Any + Send + Sync,
    {
        self.intercept(name, Value::new(config))
    }

    /// Return every intercept config registered for `name`, root-most first.
    pub fn intercepts(&self, name: &str) -> Vec<Value> {
        self.intercepts.get(name).cloned().unwrap_or_default()
    }

    /// Return the most recent (closest) intercept config for `name`, if any.
    pub fn intercept_value(&self, name: &str) -> Option<Value> {
        self.intercepts
            .get(name)
            .and_then(|configs| configs.last())
            .cloned()
    }

    fn bus_or_panic(&self) -> &EventBus {
        self.bus.as_ref().expect(
            "Context has no EventBus; call Context::with_event_bus or boot a PluginRegistry",
        )
    }

    fn registry_root(&self) -> Option<Arc<RegistryState>> {
        self.inner
            .registry
            .lock()
            .expect("registry pointer poisoned")
            .as_ref()
            .and_then(|weak| weak.upgrade())
    }

    /// Register an asynchronous listener owned by this context.
    ///
    /// The listener participates in `*_scoped` target filtering using this
    /// context as its owner. Prefer this over [`EventBus::on`] for listeners
    /// that should be scoped to a particular context. When this context is
    /// scoped to a provider fiber the registration is adopted by that fiber,
    /// so the listener is removed when the fiber reloads or disposes (see
    /// [`Context::effect`]); returning the handle from a plugin's `apply` is
    /// therefore optional but harmless.
    pub fn on<E, F>(&self, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> crate::BoxFuture<Result<Value, EventError>> + Send + Sync + 'static,
    {
        let handle = self.bus_or_panic().on_owned(self, listener);
        self.track_effect(handle.clone());
        handle
    }

    pub fn on_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> crate::BoxFuture<Result<Value, EventError>> + Send + Sync + 'static,
    {
        let handle = self.bus_or_panic().on_with_owned(self, listener, options);
        self.track_effect(handle.clone());
        handle
    }

    pub fn on_async<E, F>(&self, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> crate::BoxFuture<Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.on(listener)
    }

    pub fn on_async_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> crate::BoxFuture<Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.on_with(listener, options)
    }

    pub fn once<E, F>(&self, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> crate::BoxFuture<Result<Value, EventError>> + Send + Sync + 'static,
    {
        let handle = self.bus_or_panic().once_owned(self, listener);
        self.track_effect(handle.clone());
        handle
    }

    pub fn once_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> crate::BoxFuture<Result<Value, EventError>> + Send + Sync + 'static,
    {
        let handle = self.bus_or_panic().once_with_owned(self, listener, options);
        self.track_effect(handle.clone());
        handle
    }

    pub fn once_async<E, F>(&self, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> crate::BoxFuture<Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.once(listener)
    }

    pub fn once_async_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> crate::BoxFuture<Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.once_with(listener, options)
    }

    pub fn on_sync<E, F>(&self, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static,
    {
        let handle = self.bus_or_panic().on_sync_owned(self, listener);
        self.track_effect(handle.clone());
        handle
    }

    pub fn on_sync_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static,
    {
        let handle = self
            .bus_or_panic()
            .on_sync_with_owned(self, listener, options);
        self.track_effect(handle.clone());
        handle
    }

    pub fn once_sync<E, F>(&self, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static,
    {
        let handle = self.bus_or_panic().once_sync_owned(self, listener);
        self.track_effect(handle.clone());
        handle
    }

    pub fn once_sync_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static,
    {
        let handle = self
            .bus_or_panic()
            .once_sync_with_owned(self, listener, options);
        self.track_effect(handle.clone());
        handle
    }

    /// Register a typed waterfall listener owned by this context.
    pub fn on_waterfall<E, F>(&self, listener: F) -> EffectHandle
    where
        E: WaterfallEvent,
        F: Fn(Arc<E>, Next<E::Output>) -> crate::BoxFuture<Result<E::Output, EventError>>
            + Send
            + Sync
            + 'static,
    {
        let handle = self.bus_or_panic().on_waterfall_owned(self, listener);
        self.track_effect(handle.clone());
        handle
    }

    /// Compose typed waterfall listeners around an innermost callback.
    pub async fn waterfall<E, F>(&self, event: Arc<E>, inner: F) -> Result<E::Output, EventError>
    where
        E: WaterfallEvent,
        F: Fn() -> crate::BoxFuture<Result<E::Output, EventError>> + Send + Sync + 'static,
    {
        self.bus_or_panic().waterfall(event, inner).await
    }

    /// Dispatch an event without a target context.
    ///
    /// This runs every non-waterfall listener, ordered or otherwise, exactly
    /// like [`EventBus::emit`]. Use [`EventBus::emit_scoped`] to filter listeners
    /// by an explicit target context.
    pub fn emit<E: Event>(&self, event: Arc<E>) {
        self.bus_or_panic().emit(event)
    }

    pub fn emit_sync<E: Event>(&self, event: Arc<E>) -> Vec<Result<Value, EventError>> {
        self.bus_or_panic().emit_sync(event)
    }

    pub async fn serial<E: Event>(&self, event: Arc<E>) -> Result<Option<Value>, EventError> {
        self.bus_or_panic().serial(event).await
    }

    pub async fn serial_collect<E: Event>(&self, event: Arc<E>) -> Vec<Result<Value, EventError>> {
        self.bus_or_panic().serial_collect(event).await
    }

    pub async fn serial_strict<E: Event>(&self, event: Arc<E>) -> Result<Vec<Value>, EventError> {
        self.bus_or_panic().serial_strict(event).await
    }

    pub async fn parallel<E: Event>(&self, event: Arc<E>) -> Result<(), EventError> {
        self.bus_or_panic().parallel(event).await
    }

    pub async fn bail<E: Event>(&self, event: Arc<E>) -> Result<Option<Value>, EventError> {
        self.bus_or_panic().bail(event).await
    }

    pub fn bail_sync<E: Event>(&self, event: Arc<E>) -> Result<Option<Value>, EventError> {
        self.bus_or_panic().bail_sync(event)
    }

    pub fn plugin(&self, child: impl Plugin) -> crate::BoxFuture<anyhow::Result<PluginHandle>> {
        self.plugin_with_config(child, Config::new(()))
    }

    pub fn plugin_with_config(
        &self,
        child: impl Plugin,
        config: Config,
    ) -> crate::BoxFuture<anyhow::Result<PluginHandle>> {
        let child: Arc<dyn Plugin> = Arc::new(child);
        let parent = self.provider.upgrade();

        // Primary path: dynamic runtime registry attached to this context. A
        // plugin started without a current fiber becomes a top-level runtime
        // entry; inside another plugin's `apply` it mounts under that fiber.
        if let Some(registry) = self.registry_root() {
            let registry = Arc::clone(&registry);
            return Box::pin(async move { registry.spawn_plugin(child, parent, config).await });
        }

        // Legacy fallback for contexts wired with a `Spawner` only. It always
        // requires a current fiber because the spawner has no notion of a
        // top-level runtime entry.
        let parent = match parent {
            Some(parent) => parent,
            None => {
                return Box::pin(async {
                    anyhow::bail!("no current fiber to mount a plugin into")
                });
            }
        };
        match &self.spawner {
            Some(spawner) => {
                let spawner = Arc::clone(spawner);
                spawner(child, parent, config)
            }
            None => Box::pin(async { anyhow::bail!("no plugin runtime available") }),
        }
    }

    /// Remove a running plugin instance by its stable key.
    ///
    /// The instance is disposed and stops being reconciled. Dependents are
    /// notified through the normal service lifecycle and re-evaluate their
    /// dependencies. Returns `false` when no live instance has that key.
    pub fn remove_plugin(&self, key: PluginKey) -> crate::BoxFuture<bool> {
        let Some(registry) = self.registry_root() else {
            return Box::pin(async { false });
        };
        let registry = Arc::clone(&registry);
        Box::pin(async move { registry.remove_plugin(key).await })
    }

    /// Replace the plugin instance identified by `old` with a new plugin.
    ///
    /// The old instance is removed and disposed, then the new plugin is
    /// started with the same ownership context as a fresh [`Context::plugin`]
    /// call would have.
    pub fn replace_plugin(
        &self,
        old: PluginKey,
        new: impl Plugin,
    ) -> crate::BoxFuture<anyhow::Result<PluginHandle>> {
        self.replace_plugin_with_config(old, new, Config::new(()))
    }

    /// [`Context::replace_plugin`] with explicit configuration.
    pub fn replace_plugin_with_config(
        &self,
        old: PluginKey,
        new: impl Plugin,
        config: Config,
    ) -> crate::BoxFuture<anyhow::Result<PluginHandle>> {
        let new: Arc<dyn Plugin> = Arc::new(new);
        let parent = self.provider.upgrade();
        let registry = match self.registry_root() {
            Some(registry) => registry,
            None => {
                return Box::pin(async { anyhow::bail!("no plugin runtime available") });
            }
        };
        Box::pin(async move {
            registry.remove_plugin(old).await;
            registry.spawn_plugin(new, parent, config).await
        })
    }

    pub fn provide<T: TypedService>(&self, value: Arc<T>) -> Result<EffectHandle, ContextError> {
        self.provide_named::<T>(T::NAME.to_owned(), value)
    }

    pub fn provide_named<T: Service>(
        &self,
        name: String,
        value: Arc<T>,
    ) -> Result<EffectHandle, ContextError> {
        let value = Value::from_arc(value);
        let type_name = std::any::type_name::<T>();
        let check: Option<AvailabilityCheck> = Some(Arc::new(move |value: &Value| {
            value
                .downcast::<T>()
                .map(|s| s.is_available())
                .unwrap_or(false)
        }));
        self.provide_entry(name, value, type_name, check)
    }

    /// Provides a service under a runtime string key without requiring the
    /// value type to implement [`Service`].
    ///
    /// This is the escape hatch for dynamic/external plugins that only know
    /// their service names and payloads at runtime. Availability is assumed to
    /// be immediate; use [`Context::provide`] for typed services that need the
    /// [`Service::is_available`] gate.
    pub fn provide_any(
        &self,
        name: impl Into<String>,
        value: Value,
    ) -> Result<EffectHandle, ContextError> {
        let name = name.into();
        let type_name = value.type_name();
        self.provide_entry(name, value, type_name, None)
    }

    /// Provides a `dyn Service` value under an explicit runtime name.
    ///
    /// This is the dyn counterpart of [`Context::provide_named`]: the value is
    /// stored through a private wrapper so it rides in `Value`'s concrete-`Any`
    /// storage, and the [`Service::is_available`] gate keeps gating
    /// `dependency_epoch`. Resolve it back with [`Context::require_dyn`].
    pub fn provide_dyn(
        &self,
        name: impl Into<String>,
        value: Arc<dyn Service>,
    ) -> Result<EffectHandle, ContextError> {
        let name = name.into();
        let type_name = std::any::type_name::<Arc<dyn Service>>();
        let check: Option<AvailabilityCheck> = Some(Arc::new(|value: &Value| {
            value
                .downcast::<DynServiceValue>()
                .map(|wrapper| wrapper.0.is_available())
                .unwrap_or(false)
        }));
        self.provide_entry(name, Value::new(DynServiceValue(value)), type_name, check)
    }

    /// Typed lookup of a value provided via [`Context::provide_dyn`].
    pub fn require_dyn(&self, name: &str) -> Result<Arc<dyn Service>, ContextError> {
        let scope = self
            .inner
            .scope_for(self, name)
            .ok_or_else(|| ContextError::Missing(name.to_owned()))?;
        let state = self.inner.state.lock().expect("service state poisoned");
        let entry = state
            .implementations
            .get(&scope)
            .ok_or_else(|| ContextError::Missing(name.to_owned()))?;
        entry
            .value
            .clone()
            .downcast::<DynServiceValue>()
            .map(|wrapper| wrapper.0.clone())
            .map_err(|_| ContextError::TypeMismatch {
                name: name.to_owned(),
                expected: std::any::type_name::<Arc<dyn Service>>(),
                actual: entry.type_name,
            })
    }

    fn provide_entry(
        &self,
        name: String,
        value: Value,
        type_name: &'static str,
        check: Option<AvailabilityCheck>,
    ) -> Result<EffectHandle, ContextError> {
        let key = name.clone();
        let scope = self.inner.ensure_scope(self, &name);
        let provider = self.provider.clone();
        let provider_key = fiber_key(&provider);
        let generation = self.inner.next_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let entry = Entry {
            value,
            type_name,
            generation,
            provider,
            check,
        };
        {
            // A providing fiber that is already Disposing/Disposed can never
            // adopt the un-registration cleanup (adopt rejects AND drops it):
            // the entry would be permanently unremovable and every later
            // provide of this name would answer Duplicate. Refuse up front.
            if let Some(fiber) = self.provider.upgrade() {
                if (fiber.state() as u8) >= FiberState::Disposing as u8 {
                    return Err(ContextError::ProviderDisposing(name));
                }
            }
            let mut state = self.inner.state.lock().expect("service state poisoned");
            if state.implementations.contains_key(&scope) {
                return Err(ContextError::Duplicate(name));
            }
            state.implementations.insert(scope, entry);
        }
        if let Some(provider_key) = provider_key {
            self.inner
                .provided_by
                .lock()
                .expect("provided_by poisoned")
                .entry(provider_key)
                .or_default()
                .insert(key.clone());
        }
        self.inner.notify(key.as_str());

        let inner = Arc::clone(&self.inner);
        let effect = EffectHandle::new(move || {
            let removed = {
                let mut state = inner.state.lock().expect("service state poisoned");
                let stale = state
                    .implementations
                    .get(&scope)
                    .map(|entry| entry.generation == generation)
                    .unwrap_or(false);
                if stale {
                    state.implementations.remove(&scope);
                    true
                } else {
                    false
                }
            };
            if removed {
                if let Some(provider_key) = provider_key {
                    let mut by = inner.provided_by.lock().expect("provided_by poisoned");
                    if let Some(names) = by.get_mut(&provider_key) {
                        names.remove(key.as_str());
                        if names.is_empty() {
                            by.remove(&provider_key);
                        }
                    }
                }
                inner.notify(key.as_str());
            }
        });

        if let Some(fiber) = self.provider.upgrade() {
            if let Some(cleanup) = effect.into_cleanup() {
                let _ = fiber.adopt(cleanup);
            }
            Ok(EffectHandle::none())
        } else {
            Ok(effect)
        }
    }

    pub fn provide_async<T: TypedService>(
        &self,
        value: Arc<T>,
    ) -> crate::BoxFuture<Result<EffectHandle, ContextError>> {
        self.provide_named_async::<T>(T::NAME.to_owned(), value)
    }

    pub fn provide_named_async<T: Service>(
        &self,
        name: String,
        value: Arc<T>,
    ) -> crate::BoxFuture<Result<EffectHandle, ContextError>> {
        let ctx = self.clone();
        let bus = self.bus.clone();

        Box::pin(async move {
            let Some(bus) = bus else {
                return ctx.provide_named::<T>(name, value);
            };

            if internal_set_allowed(&bus, &ctx, &name, Value::from_arc(value.clone())).await? {
                ctx.provide_named::<T>(name, value)
            } else {
                Ok(EffectHandle::none())
            }
        })
    }

    /// Async dynamic-service registration.
    ///
    /// Routes through `internal/set` when a bus is attached so external
    /// listeners can veto the write; otherwise it falls back to the
    /// synchronous [`Context::provide_any`].
    pub fn provide_any_async(
        &self,
        name: String,
        value: Value,
    ) -> crate::BoxFuture<Result<EffectHandle, ContextError>> {
        let ctx = self.clone();
        let bus = self.bus.clone();

        Box::pin(async move {
            let Some(bus) = bus else {
                return ctx.provide_any(name, value);
            };

            if internal_set_allowed(&bus, &ctx, &name, value.clone()).await? {
                ctx.provide_any(name, value)
            } else {
                Ok(EffectHandle::none())
            }
        })
    }

    pub fn provided_names(&self, fiber: &Arc<Fiber>) -> HashSet<String> {
        let key = Arc::as_ptr(fiber) as usize;
        self.inner
            .provided_by
            .lock()
            .expect("provided_by poisoned")
            .get(&key)
            .cloned()
            .unwrap_or_default()
    }

    fn resolve_value(&self, name: &str) -> Option<Value> {
        let scope = self.inner.scope_for(self, name)?;
        let state = self.inner.state.lock().expect("service state poisoned");
        state
            .implementations
            .get(&scope)
            .map(|entry| entry.value.clone())
    }

    /// Looks up a service by a runtime string key and returns its raw payload.
    ///
    /// Unlike [`Context::require`], this does not require the caller to know
    /// the concrete type up front and does not fail on a type mismatch; callers
    /// downcast the returned [`Value`] themselves.
    pub fn require_any(&self, name: &str) -> Option<Value> {
        self.resolve_value(name)
    }

    pub fn require<T: TypedService>(&self) -> Result<Arc<T>, ContextError> {
        self.require_named::<T>(T::NAME)
    }

    pub fn require_named<T: Service>(&self, name: &str) -> Result<Arc<T>, ContextError> {
        let scope = self
            .inner
            .scope_for(self, name)
            .ok_or_else(|| ContextError::Missing(name.to_owned()))?;
        let state = self.inner.state.lock().expect("service state poisoned");
        let entry = state
            .implementations
            .get(&scope)
            .ok_or_else(|| ContextError::Missing(name.to_owned()))?;
        entry
            .value
            .clone()
            .downcast::<T>()
            .map_err(|_| ContextError::TypeMismatch {
                name: name.to_owned(),
                expected: std::any::type_name::<T>(),
                actual: entry.type_name,
            })
    }

    pub fn require_async<T: TypedService>(&self) -> crate::BoxFuture<Result<Arc<T>, ContextError>> {
        self.require_named_async::<T>(T::NAME)
    }

    pub fn require_named_async<T: Service>(
        &self,
        name: &str,
    ) -> crate::BoxFuture<Result<Arc<T>, ContextError>> {
        let name = name.to_owned();
        let ctx = self.clone();
        let bus = self.bus.clone();

        Box::pin(async move {
            let Some(bus) = bus else {
                return ctx.require_named::<T>(&name);
            };

            let outcome = internal_get_value(&bus, &ctx, &name).await?;
            match outcome {
                Some(value) => value
                    .downcast::<T>()
                    .map_err(|_| ContextError::TypeMismatch {
                        name: name.clone(),
                        expected: std::any::type_name::<T>(),
                        actual: value.type_name(),
                    }),
                None => Err(ContextError::Missing(name)),
            }
        })
    }

    /// Async dynamic-service lookup.
    ///
    /// Routes through `internal/get` when a bus is attached so external
    /// listeners can virtualize or short-circuit the result; otherwise it falls
    /// back to the synchronous [`Context::require_any`].
    pub fn require_any_async(
        &self,
        name: String,
    ) -> crate::BoxFuture<Result<Option<Value>, ContextError>> {
        let ctx = self.clone();
        let bus = self.bus.clone();

        Box::pin(async move {
            let Some(bus) = bus else {
                return Ok(ctx.resolve_value(&name));
            };

            internal_get_value(&bus, &ctx, &name).await
        })
    }

    pub fn has(&self, name: &str) -> bool {
        let Some(scope) = self.inner.scope_for(self, name) else {
            return false;
        };
        let state = self.inner.state.lock().expect("service state poisoned");
        state.implementations.contains_key(&scope)
    }

    pub fn dependency_epoch(&self, inject: &[&str]) -> Option<Vec<u64>> {
        let state = self.inner.state.lock().expect("service state poisoned");
        let mut epoch = Vec::with_capacity(inject.len());
        for name in inject {
            let scope = self
                .isolates
                .get(*name)
                .copied()
                .or_else(|| state.default_scopes.get(*name).copied())?;
            let entry = state.implementations.get(&scope)?;
            if let Some(provider) = entry.provider.upgrade() {
                if provider.state() != FiberState::Active {
                    return None;
                }
            }
            if let Some(check) = &entry.check {
                if !check(&entry.value) {
                    return None;
                }
            }
            epoch.push(entry.generation);
        }
        Some(epoch)
    }

    pub fn subscribe(&self, callback: Subscriber) -> EffectHandle {
        let id = self.inner.next_subscriber_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.inner
            .subscribers
            .lock()
            .expect("subscriber list poisoned")
            .push((id, callback));
        let inner = Arc::clone(&self.inner);
        EffectHandle::new(move || {
            if let Ok(mut subs) = inner.subscribers.lock() {
                subs.retain(|(sid, _)| *sid != id);
            }
        })
    }
}

async fn internal_set_allowed(
    bus: &EventBus,
    ctx: &Context,
    name: &str,
    value: Value,
) -> Result<bool, ContextError> {
 // `error` is a prebuilt placeholder matching `internal/set`
    // signature; the innermost provider does not consume it.
    let args = vec![
        Value::new(ctx.clone()),
        Value::new(name.to_owned()),
        value,
        Value::new(ContextError::Missing(name.to_owned())),
    ];
    let inner = || -> crate::BoxFuture<Result<Option<Value>, EventError>> {
        Box::pin(async { Ok(Some(Value::new(true))) })
    };

    let outcome = bus.dynamic().waterfall("internal/set", args, inner).await?;
    Ok(outcome
        .and_then(|v| v.downcast::<bool>().ok())
        .map(|b| *b)
        .unwrap_or(false))
}

async fn internal_get_value(
    bus: &EventBus,
    ctx: &Context,
    name: &str,
) -> Result<Option<Value>, ContextError> {
    let lookup_ctx = ctx.clone();
    let lookup_name = name.to_owned();
    let args = vec![
        Value::new(ctx.clone()),
        Value::new(name.to_owned()),
        Value::new(ContextError::Missing(name.to_owned())),
    ];
    let inner = move || -> crate::BoxFuture<Result<Option<Value>, EventError>> {
        let lookup_ctx = lookup_ctx.clone();
        let lookup_name = lookup_name.clone();
        Box::pin(async move { Ok(lookup_ctx.resolve_value(&lookup_name)) })
    };

    Ok(bus.dynamic().waterfall("internal/get", args, inner).await?)
}

fn fiber_key(provider: &Weak<Fiber>) -> Option<usize> {
    provider.upgrade().map(|f| Arc::as_ptr(&f) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{Service, TypedService};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    struct Ping;
    impl Event for Ping {
        const NAME: &'static str = "ctx/ping";
    }

    struct Counter(usize);
    impl TypedService for Counter {
        const NAME: &'static str = "counter";
    }

    #[test]
    fn provide_and_require_roundtrip() {
        let ctx = Context::new();
        let _h = ctx.provide::<Counter>(Arc::new(Counter(7)));
        let got = ctx.require::<Counter>().unwrap();
        assert_eq!(got.0, 7);
    }

    #[test]
    fn require_missing_is_error() {
        let ctx = Context::new();
        assert!(matches!(
            ctx.require::<Counter>(),
            Err(ContextError::Missing(ref name)) if name.as_str() == "counter"
        ));
    }

    #[test]
    fn require_wrong_type_is_error_not_panic() {
        struct CounterImpostor;
        impl TypedService for CounterImpostor {
            const NAME: &'static str = "counter";
        }

        let ctx = Context::new();
        let _h = ctx.provide::<CounterImpostor>(Arc::new(CounterImpostor));
        assert!(matches!(
            ctx.require::<Counter>(),
            Err(ContextError::TypeMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn disposing_effect_removes_service() {
        let ctx = Context::new();
        let h = ctx.provide::<Counter>(Arc::new(Counter(3))).unwrap();
        assert!(ctx.has("counter"));
        h.dispose_async().await;
        assert!(!ctx.has("counter"));
    }

    #[test]
    fn provide_duplicate_is_error() {
        let ctx = Context::new();
        let _a = ctx.provide::<Counter>(Arc::new(Counter(1))).unwrap();
        assert!(matches!(
            ctx.provide::<Counter>(Arc::new(Counter(2))),
            Err(ContextError::Duplicate(ref name)) if name.as_str() == "counter"
        ));
        assert_eq!(ctx.require::<Counter>().unwrap().0, 1);
    }

    #[tokio::test]
    async fn provide_after_dispose_is_allowed() {
        let ctx = Context::new();
        let first = ctx.provide::<Counter>(Arc::new(Counter(1))).unwrap();
        first.dispose_async().await;
        let _second = ctx.provide::<Counter>(Arc::new(Counter(2))).unwrap();
        assert_eq!(ctx.require::<Counter>().unwrap().0, 2);
    }

    #[test]
    fn contexts_are_isolated() {
        let a = Context::new();
        let b = Context::new();
        let _h = a.provide::<Counter>(Arc::new(Counter(9)));
        assert!(a.has("counter"));
        assert!(!b.has("counter"));
    }

    #[test]
    fn require_named_looks_up_by_explicit_key() {
        let ctx = Context::new();
        let _h = ctx.provide::<Counter>(Arc::new(Counter(3)));
        assert_eq!(ctx.require_named::<Counter>("counter").unwrap().0, 3);
        assert!(matches!(
            ctx.require_named::<Counter>("nope"),
            Err(ContextError::Missing(ref name)) if name.as_str() == "nope"
        ));
    }

    #[test]
    fn seam_prefix_detection() {
        assert!(is_public_seam("seam:llm"));
        assert!(!is_public_seam("kernel:foo"));
    }

    #[tokio::test]
    async fn dependency_epoch_tracks_generation() {
        let ctx = Context::new();
        assert_eq!(ctx.dependency_epoch(&["counter"]), None);
        let a = ctx.provide::<Counter>(Arc::new(Counter(1))).unwrap();
        let epoch1 = ctx.dependency_epoch(&["counter"]).unwrap();
        a.dispose_async().await;
        let _b = ctx.provide::<Counter>(Arc::new(Counter(2))).unwrap();
        let epoch2 = ctx.dependency_epoch(&["counter"]).unwrap();
        assert_ne!(epoch1, epoch2);
    }

    #[test]
    fn provider_fiber_gates_dependency_epoch() {
        let ctx = Context::new();
        let fiber = Fiber::new();
        let scoped = ctx.scoped(Arc::downgrade(&fiber));
        let _h = scoped.provide::<Counter>(Arc::new(Counter(1)));
        assert_eq!(ctx.dependency_epoch(&["counter"]), None);
        fiber.set_loading();
        assert_eq!(ctx.dependency_epoch(&["counter"]), None);
        fiber.set_active();
        assert!(ctx.dependency_epoch(&["counter"]).is_some());
    }

    #[test]
    fn failed_provider_fiber_blocks_dependency_epoch_until_retry() {
        let ctx = Context::new();
        let fiber = Fiber::new();
        let scoped = ctx.scoped(Arc::downgrade(&fiber));
        fiber.set_loading();
        let _h = scoped.provide::<Counter>(Arc::new(Counter(1)));
        fiber.set_failed();
        // A failed provider must gate dependents exactly like a pending one.
        assert_eq!(ctx.dependency_epoch(&["counter"]), None);
        // The reconcile retry path goes Failed -> Loading -> Active.
        assert!(fiber.set_loading());
        assert!(fiber.set_active());
        assert!(ctx.dependency_epoch(&["counter"]).is_some());
    }

    #[test]
    fn unavailable_service_blocks_dependency_epoch() {
        struct Gate(Arc<AtomicBool>);
        impl TypedService for Gate {
            const NAME: &'static str = "gate";
            fn is_available(&self) -> bool {
                self.0.load(Ordering::SeqCst)
            }
        }

        let ctx = Context::new();
        let flag = Arc::new(AtomicBool::new(false));
        let _h = ctx.provide::<Gate>(Arc::new(Gate(Arc::clone(&flag))));
        assert_eq!(ctx.dependency_epoch(&["gate"]), None);
        flag.store(true, Ordering::SeqCst);
        assert!(ctx.dependency_epoch(&["gate"]).is_some());
    }

    #[test]
    fn provided_names_tracks_provider_fiber() {
        let ctx = Context::new();
        let fiber = Fiber::new();
        let scoped = ctx.scoped(Arc::downgrade(&fiber));
        let _h = scoped.provide::<Counter>(Arc::new(Counter(1)));
        let names = ctx.provided_names(&fiber);
        assert_eq!(names.len(), 1);
        assert!(names.contains("counter"));
    }

    #[tokio::test]
    async fn scoped_provide_auto_adopts_into_fiber() {
        let ctx = Context::new();
        let fiber = Fiber::new();
        let scoped = ctx.scoped(Arc::downgrade(&fiber));
        let _h = scoped.provide::<Counter>(Arc::new(Counter(1)));
        assert!(ctx.provided_names(&fiber).contains("counter"));
        fiber.dispose().await;
        assert!(ctx.provided_names(&fiber).is_empty());
    }

    #[test]
    fn dynamic_service_names_roundtrip() {
        let ctx = Context::new();
        let _h = ctx.provide_named::<Counter>("tenant:123".to_owned(), Arc::new(Counter(9)));
        assert_eq!(ctx.require_named::<Counter>("tenant:123").unwrap().0, 9);
        assert!(ctx.has("tenant:123"));
    }

    #[test]
    fn provide_named_duplicate_is_error() {
        let ctx = Context::new();
        let _a = ctx
            .provide_named::<Counter>("dyn".to_owned(), Arc::new(Counter(1)))
            .unwrap();
        assert!(matches!(
            ctx.provide_named::<Counter>("dyn".to_owned(), Arc::new(Counter(2))),
            Err(ContextError::Duplicate(ref name)) if name == "dyn"
        ));
    }

    #[test]
    fn multiple_subscribers_are_notified() {
        let ctx = Context::new();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let s1 = Arc::clone(&seen);
        let s2 = Arc::clone(&seen);
        let _sub1 = ctx.subscribe(Arc::new(move |name| {
            s1.lock().unwrap().push(name.to_owned());
        }));
        let _sub2 = ctx.subscribe(Arc::new(move |name| {
            s2.lock().unwrap().push(name.to_owned());
        }));
        let _h = ctx.provide::<Counter>(Arc::new(Counter(1)));
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn new_isolation_labels_are_unique() {
        let ctx = Context::new();
        let a = ctx.new_isolation();
        let b = ctx.new_isolation();
        assert_ne!(a, b);
        assert_eq!(Isolation::from_raw(a.as_raw()), a);
    }

    #[test]
    fn isolate_creates_independent_empty_slot() {
        let ctx = Context::new();
        let _root = ctx.provide::<Counter>(Arc::new(Counter(1)));
        let child = ctx.isolate("counter");
        assert!(!child.has("counter"));
        assert!(matches!(
            child.require::<Counter>(),
            Err(ContextError::Missing(ref name)) if name.as_str() == "counter"
        ));
        let _child_svc = child.provide::<Counter>(Arc::new(Counter(2)));
        assert_eq!(child.require::<Counter>().unwrap().0, 2);
        assert_eq!(ctx.require::<Counter>().unwrap().0, 1);
    }

    #[test]
    fn isolate_does_not_leak_to_parent() {
        let ctx = Context::new();
        let child = ctx.isolate("counter");
        let _h = child.provide::<Counter>(Arc::new(Counter(5)));
        assert!(!ctx.has("counter"));
        assert!(child.has("counter"));
    }

    #[test]
    fn isolate_with_joins_scope() {
        let ctx = Context::new();
        let label = ctx.new_isolation();
        let a = ctx.isolate_with("counter", label);
        let b = ctx.isolate_with("counter", label);
        let _h = a.provide::<Counter>(Arc::new(Counter(7)));
        assert!(b.has("counter"));
        assert!(!ctx.has("counter"));
    }

    #[test]
    fn isolate_only_overrides_the_named_service() {
        let ctx = Context::new();
        let _root = ctx.provide::<Counter>(Arc::new(Counter(1)));

        struct Other(usize);
        impl TypedService for Other {
            const NAME: &'static str = "other";
        }
        let _other = ctx.provide::<Other>(Arc::new(Other(9)));

        // Isolating only "counter" leaves "other" resolving through the root.
        let child = ctx.isolate("counter");
        assert!(!child.has("counter"));
        assert_eq!(child.require::<Other>().unwrap().0, 9);
    }

    #[test]
    fn isolate_many_shares_one_label() {
        let ctx = Context::new();
        let label = ctx.new_isolation();
        let joined = ctx.isolate_with("counter", label);
        let many = ctx.isolate_many(["counter"], Some(label));
        let _h = joined.provide::<Counter>(Arc::new(Counter(7)));
        assert!(many.has("counter"));
        assert!(!ctx.has("counter"));
    }

    #[test]
    fn isolate_many_shared_slot_second_name_is_duplicate_error() {
        let ctx = Context::new();
        let many = ctx.isolate_many(["counter", "mirror"], None);
        let _h = many.provide::<Counter>(Arc::new(Counter(7)));
        // "mirror" maps to the same slot as "counter", so providing it must
        // fail instead of silently replacing the shared implementation.
        assert!(matches!(
            many.provide_named::<Counter>("mirror".to_owned(), Arc::new(Counter(2))),
            Err(ContextError::Duplicate(ref name)) if name == "mirror"
        ));
        // The failed write must not clobber the original entry.
        assert_eq!(many.require::<Counter>().unwrap().0, 7);
    }

    #[test]
    fn isolate_many_joined_branch_duplicate_is_error() {
        let ctx = Context::new();
        let label = ctx.new_isolation();
        let joined = ctx.isolate_with("counter", label);
        let many = ctx.isolate_many(["mirror"], Some(label));
        let _h = joined.provide::<Counter>(Arc::new(Counter(7)));
        assert!(matches!(
            many.provide_named::<Counter>("mirror".to_owned(), Arc::new(Counter(2))),
            Err(ContextError::Duplicate(ref name)) if name == "mirror"
        ));
        // The shared slot still resolves the original implementation through
        // the joined alias name.
        assert_eq!(many.require_named::<Counter>("mirror").unwrap().0, 7);
    }

    #[tokio::test]
    async fn subscribe_is_notified_on_provide_and_remove() {
        let ctx = Context::new();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let s = Arc::clone(&seen);
        let _sub = ctx.subscribe(Arc::new(move |name| {
            s.lock().unwrap().push(name.to_owned());
        }));
        let h = ctx.provide::<Counter>(Arc::new(Counter(1))).unwrap();
        h.dispose_async().await;
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["counter".to_owned(), "counter".to_owned()]
        );
    }

    #[tokio::test]
    async fn context_and_fiber_are_safe_across_tasks() {
        let ctx = Arc::new(Context::new());
        let mut handles = Vec::new();
        for i in 0..8 {
            let ctx = Arc::clone(&ctx);
            handles.push(tokio::spawn(async move {
                for _ in 0..100 {
                    let h = ctx.provide::<Counter>(Arc::new(Counter(i))).unwrap();
                    let _ = ctx.require::<Counter>();
                    let fiber = Fiber::new();
                    let _ = fiber.adopt(Box::new(|| Box::pin(async {})));
                    fiber.dispose().await;
                    h.dispose_async().await;
                }
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
    }

    #[tokio::test]
    async fn subscribe_cleanup_removes_subscriber() {
        let ctx = Context::new();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let s = Arc::clone(&seen);
        let sub = ctx.subscribe(Arc::new(move |name| {
            s.lock().unwrap().push(name.to_owned());
        }));
        sub.dispose_async().await;
        let _h = ctx.provide::<Counter>(Arc::new(Counter(1)));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn dependency_epoch_empty_inject_is_some_empty() {
        let ctx = Context::new();
        assert_eq!(ctx.dependency_epoch(&[]), Some(vec![]));
    }

    #[test]
    fn dependency_epoch_requires_all_dependencies() {
        let ctx = Context::new();
        let _h = ctx.provide::<Counter>(Arc::new(Counter(1)));
        assert_eq!(ctx.dependency_epoch(&["counter", "missing"]), None);
    }

    #[tokio::test]
    async fn scoped_context_keeps_event_bus() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);
        let fiber = Fiber::new();
        let scoped = ctx.scoped(Arc::downgrade(&fiber));
        let _h = scoped.provide::<Counter>(Arc::new(Counter(3)));
        let _intercept = bus.dynamic().on_waterfall("internal/get", |_event, next| {
            Box::pin(async move { next().await })
        });
        let got = scoped
            .require_named_async::<Counter>("counter")
            .await
            .unwrap();
        assert_eq!(got.0, 3);
    }

    #[tokio::test]
    async fn require_named_async_without_bus_uses_direct_lookup() {
        let ctx = Context::new();
        let _h = ctx.provide::<Counter>(Arc::new(Counter(9)));
        let got = ctx.require_named_async::<Counter>("counter").await.unwrap();
        assert_eq!(got.0, 9);
    }

    #[tokio::test]
    async fn require_named_async_routes_through_internal_get() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);
        let _h = ctx.provide::<Counter>(Arc::new(Counter(7)));

        let _intercept = bus.dynamic().on_waterfall("internal/get", |event, next| {
            Box::pin(async move {
                assert_eq!(event.name(), "internal/get");
                assert_eq!(*event.args()[1].downcast::<String>().unwrap(), "counter");
                let _ = next().await?;
                Ok(Some(Value::new(Counter(999))))
            })
        });

        let got = ctx.require_named_async::<Counter>("counter").await.unwrap();
        assert_eq!(got.0, 999);
    }

    #[tokio::test]
    async fn require_named_async_missing_returns_error() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);
        assert!(matches!(
            ctx.require_named_async::<Counter>("nope").await,
            Err(ContextError::Missing(ref name)) if name.as_str() == "nope"
        ));
    }

    #[tokio::test]
    async fn provide_named_async_allows_when_internal_set_allows() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);
        let _intercept = bus.dynamic().on_waterfall("internal/set", |_event, next| {
            Box::pin(async move { next().await })
        });

        let h = ctx
            .provide_named_async::<Counter>("counter".to_owned(), Arc::new(Counter(5)))
            .await
            .unwrap();
        assert!(ctx.has("counter"));
        assert_eq!(ctx.require::<Counter>().unwrap().0, 5);
        h.dispose_async().await;
        assert!(!ctx.has("counter"));
    }

    #[tokio::test]
    async fn provide_named_async_vetoes_when_internal_set_returns_false() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);
        let _intercept = bus.dynamic().on_waterfall("internal/set", |_event, _next| {
            Box::pin(async { Ok(Some(Value::new(false))) })
        });

        let _h = ctx
            .provide_named_async::<Counter>("counter".to_owned(), Arc::new(Counter(5)))
            .await
            .unwrap();
        assert!(!ctx.has("counter"));
    }

    #[test]
    fn provide_any_and_require_any_roundtrip() {
        let ctx = Context::new();
        let _h = ctx.provide_any("dyn:counter", Value::new(42u64));
        let got = ctx.require_any("dyn:counter").unwrap();
        assert_eq!(*got.downcast::<u64>().unwrap(), 42);
    }

    #[test]
    fn require_any_missing_returns_none() {
        let ctx = Context::new();
        assert!(ctx.require_any("nope").is_none());
    }

    #[test]
    fn provide_any_wrong_type_downcast_is_error_not_panic() {
        let ctx = Context::new();
        let _h = ctx.provide_any("dyn:counter", Value::new(42u64));
        let got = ctx.require_any("dyn:counter").unwrap();
        assert!(got.downcast::<String>().is_err());
    }

    #[test]
    fn provide_any_duplicate_is_error() {
        let ctx = Context::new();
        let _a = ctx.provide_any("dyn", Value::new(1u64)).unwrap();
        assert!(matches!(
            ctx.provide_any("dyn", Value::new(2u64)),
            Err(ContextError::Duplicate(ref name)) if name == "dyn"
        ));
    }

    #[test]
    fn service_trait_is_object_safe() {
        // E0038 regression lock: `dyn Service` must be constructible.
        const _: fn(Arc<dyn Service>) = |_| {};

        struct RuntimeNamed;
        impl Service for RuntimeNamed {
            fn name(&self) -> &'static str {
                "runtime:named"
            }
        }
        let value: Arc<dyn Service> = Arc::new(RuntimeNamed);
        assert_eq!(Service::name(value.as_ref()), "runtime:named");
    }

    #[test]
    fn provide_named_works_for_runtime_named_services() {
        struct RuntimeNamed;
        impl Service for RuntimeNamed {
            fn name(&self) -> &'static str {
                "runtime:named"
            }
        }

        let ctx = Context::new();
        let _h =
            ctx.provide_named::<RuntimeNamed>("runtime:named".to_owned(), Arc::new(RuntimeNamed));
        let got = ctx.require_named::<RuntimeNamed>("runtime:named").unwrap();
        assert_eq!(got.name(), "runtime:named");
    }

    #[test]
    fn provide_dyn_and_require_dyn_roundtrip() {
        struct RuntimeNamed(bool);
        impl Service for RuntimeNamed {
            fn name(&self) -> &'static str {
                "runtime:named"
            }
            fn is_available(&self) -> bool {
                self.0
            }
        }

        let ctx = Context::new();
        let _h = ctx
            .provide_dyn("runtime:named", Arc::new(RuntimeNamed(true)))
            .unwrap();
        let got = ctx.require_dyn("runtime:named").unwrap();
        assert_eq!(got.name(), "runtime:named");
        assert!(ctx.dependency_epoch(&["runtime:named"]).is_some());
        assert!(matches!(
            ctx.require_dyn("nope"),
            Err(ContextError::Missing(ref name)) if name == "nope"
        ));

        // The availability gate applies to dyn services too.
        let _gated = ctx
            .provide_dyn("gated", Arc::new(RuntimeNamed(false)))
            .unwrap();
        assert_eq!(ctx.dependency_epoch(&["gated"]), None);
    }

    #[test]
    fn require_dyn_type_mismatch_on_typed_slot() {
        let ctx = Context::new();
        let _h = ctx.provide::<Counter>(Arc::new(Counter(1)));
        assert!(matches!(
            ctx.require_dyn("counter"),
            Err(ContextError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn typed_service_blanket_bridges_name_and_availability() {
        struct Gated(Arc<AtomicBool>);
        impl TypedService for Gated {
            const NAME: &'static str = "gated-typed";
            fn is_available(&self) -> bool {
                self.0.load(Ordering::SeqCst)
            }
        }

        let ctx = Context::new();
        let flag = Arc::new(AtomicBool::new(false));
        let _h = ctx.provide::<Gated>(Arc::new(Gated(Arc::clone(&flag))));
        assert_eq!(ctx.dependency_epoch(&["gated-typed"]), None);
        flag.store(true, Ordering::SeqCst);
        assert!(ctx.dependency_epoch(&["gated-typed"]).is_some());
        // The blanket impl bridges `name()` into `Service`.
        let got = ctx.require::<Gated>().unwrap();
        assert_eq!(Service::name(got.as_ref()), "gated-typed");
    }

    #[tokio::test]
    async fn provide_any_async_and_require_any_async_roundtrip() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);

        let h = ctx
            .provide_any_async("dyn:counter".to_owned(), Value::new(7u64))
            .await
            .unwrap();
        let got = ctx
            .require_any_async("dyn:counter".to_owned())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*got.downcast::<u64>().unwrap(), 7);

        h.dispose_async().await;
        assert!(ctx.require_any("dyn:counter").is_none());
    }

    #[tokio::test]
    async fn require_any_async_routes_through_internal_get() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);
        let _h = ctx.provide_any("dyn:counter", Value::new(7u64));

        let _intercept = bus.dynamic().on_waterfall("internal/get", |_event, next| {
            Box::pin(async move {
                let _ = next().await?;
                Ok(Some(Value::new(999u64)))
            })
        });

        let got = ctx
            .require_any_async("dyn:counter".to_owned())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*got.downcast::<u64>().unwrap(), 999);
    }

    #[tokio::test]
    async fn provide_any_async_vetoes_when_internal_set_returns_false() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);
        let _intercept = bus.dynamic().on_waterfall("internal/set", |_event, _next| {
            Box::pin(async { Ok(Some(Value::new(false))) })
        });

        let _h = ctx
            .provide_any_async("dyn:counter".to_owned(), Value::new(7u64))
            .await
            .unwrap();
        assert!(ctx.require_any("dyn:counter").is_none());
    }

    #[test]
    fn intercept_accumulates_root_most_first() {
        let ctx = Context::new();
        let child = ctx
            .intercept("llm", Value::new("root"))
            .intercept("llm", Value::new("leaf"));

        let configs = child.intercepts("llm");
        assert_eq!(configs.len(), 2);
        assert_eq!(*configs[0].downcast::<&'static str>().unwrap(), "root");
        assert_eq!(*configs[1].downcast::<&'static str>().unwrap(), "leaf");
        assert_eq!(
            *child
                .intercept_value("llm")
                .unwrap()
                .downcast::<&'static str>()
                .unwrap(),
            "leaf"
        );
        assert!(ctx.intercepts("llm").is_empty());
    }

    #[test]
    fn intercept_does_not_mutate_parent() {
        let ctx = Context::new();
        let _child = ctx.intercept("llm", Value::new("leaf"));
        assert!(ctx.intercept_value("llm").is_none());
    }

    #[test]
    fn intercept_typed_wraps_config_value() {
        let ctx = Context::new();
        let child = ctx.intercept_typed("llm", 42u64);
        assert_eq!(
            *child
                .intercept_value("llm")
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            42
        );
    }

    #[test]
    fn intercept_propagates_through_scope_clones() {
        let ctx = Context::new();
        let child = ctx.intercept("llm", Value::new("configured"));
        let scoped = child.scoped(Weak::new());
        assert_eq!(
            *scoped
                .intercept_value("llm")
                .unwrap()
                .downcast::<&'static str>()
                .unwrap(),
            "configured"
        );
    }

    #[test]
    fn with_filter_attaches_and_exposes_filter() {
        let ctx = Context::new();
        assert!(ctx.filter().is_none());
        let filtered = ctx.with_filter(|_ctx| true);
        assert!(filtered.filter().is_some());
        assert!(filtered.filter().unwrap()(&ctx));
    }

    #[tokio::test]
    async fn ctx_on_and_emit_roundtrip() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);

        let hits = Arc::new(Mutex::new(0usize));
        let h = Arc::clone(&hits);
        let _h = ctx.on::<Ping, _>(move |_| {
            let h = Arc::clone(&h);
            Box::pin(async move {
                *h.lock().unwrap() += 1;
                Ok(Value::new(()))
            })
        });

        ctx.emit(Arc::new(Ping));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(*hits.lock().unwrap(), 1);
    }

    struct Wf;
    impl Event for Wf {
        const NAME: &'static str = "ctx/wf";
    }
    impl WaterfallEvent for Wf {
        type Output = u64;
    }

    struct StubPlugin;
    impl Plugin for StubPlugin {
        fn name(&self) -> &'static str {
            "stub"
        }
    }

    #[test]
    fn context_default_creates_fresh_context() {
        let ctx = Context::default();
        assert!(!ctx.has("counter"));
    }

    #[tokio::test]
    async fn ctx_sync_event_api_roundtrip() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);
        let _h = ctx.on_sync::<Ping, _>(|_| Ok(Value::new(9u64)));

        let emitted = ctx.emit_sync(Arc::new(Ping));
        assert_eq!(emitted.len(), 1);
        assert_eq!(*emitted[0].as_ref().unwrap().downcast::<u64>().unwrap(), 9);

        let serial = ctx.serial(Arc::new(Ping)).await.unwrap().unwrap();
        assert_eq!(*serial.downcast::<u64>().unwrap(), 9);

        let bail = ctx.bail(Arc::new(Ping)).await.unwrap().unwrap();
        assert_eq!(*bail.downcast::<u64>().unwrap(), 9);

        let bail_sync = ctx.bail_sync(Arc::new(Ping)).unwrap().unwrap();
        assert_eq!(*bail_sync.downcast::<u64>().unwrap(), 9);
    }

    #[tokio::test]
    async fn ctx_waterfall_api_roundtrip() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);
        let _h = ctx.on_waterfall::<Wf, _>(|_event, next| {
            Box::pin(async move {
                let value = next().await?;
                Ok(value + 1)
            })
        });

        let out = ctx
            .waterfall(Arc::new(Wf), || Box::pin(async { Ok(1u64) }))
            .await
            .unwrap();
        assert_eq!(out, 2);
    }

    #[tokio::test]
    async fn plugin_without_runtime_returns_error() {
        let ctx = Context::new();
        let error = match ctx.plugin(StubPlugin).await {
            Ok(_) => panic!("expected plugin startup to fail"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("no current fiber"));
    }

    #[tokio::test]
    async fn remove_plugin_without_runtime_returns_false() {
        let ctx = Context::new();
        assert!(!ctx.remove_plugin(PluginKey::from_raw(1)).await);
    }

    #[tokio::test]
    async fn replace_plugin_without_runtime_returns_error() {
        let ctx = Context::new();
        let error = match ctx.replace_plugin(PluginKey::from_raw(1), StubPlugin).await {
            Ok(_) => panic!("expected plugin replacement to fail"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("no plugin runtime available"));
    }

    #[tokio::test]
    async fn provide_async_without_bus_uses_direct() {
        let ctx = Context::new();
        let handle = ctx
            .provide_async::<Counter>(Arc::new(Counter(8)))
            .await
            .unwrap();
        assert!(ctx.has("counter"));
        assert_eq!(ctx.require::<Counter>().unwrap().0, 8);
        handle.dispose_async().await;
        assert!(!ctx.has("counter"));
    }

    #[tokio::test]
    async fn provide_any_async_without_bus_uses_direct() {
        let ctx = Context::new();
        let handle = ctx
            .provide_any_async("dyn:counter".to_owned(), Value::new(7u64))
            .await
            .unwrap();
        assert!(ctx.has("dyn:counter"));
        handle.dispose_async().await;
        assert!(!ctx.has("dyn:counter"));
    }

    #[tokio::test]
    async fn require_async_without_bus_uses_direct() {
        let ctx = Context::new();
        let _h = ctx.provide::<Counter>(Arc::new(Counter(6))).unwrap();
        let got = ctx.require_async::<Counter>().await.unwrap();
        assert_eq!(got.0, 6);
    }

    #[tokio::test]
    async fn require_any_async_without_bus_uses_direct() {
        let ctx = Context::new();
        let _h = ctx.provide_any("dyn:counter", Value::new(11u64)).unwrap();
        let got = ctx
            .require_any_async("dyn:counter".to_owned())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*got.downcast::<u64>().unwrap(), 11);
    }

    #[tokio::test]
    async fn require_named_async_type_mismatch_error() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);
        let _h = ctx.provide::<Counter>(Arc::new(Counter(1))).unwrap();
        let _intercept = bus.dynamic().on_waterfall("internal/get", |_event, _next| {
            Box::pin(async { Ok(Some(Value::new("wrong type".to_owned()))) })
        });

        assert!(matches!(
            ctx.require_named_async::<Counter>("counter").await,
            Err(ContextError::TypeMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn internal_set_none_vetoes_provide() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);
        let _intercept = bus
            .dynamic()
            .on_waterfall("internal/set", |_event, _next| Box::pin(async { Ok(None) }));

        let _h = ctx
            .provide_named_async::<Counter>("counter".to_owned(), Arc::new(Counter(5)))
            .await
            .unwrap();
        assert!(!ctx.has("counter"));
    }

    #[tokio::test]
    async fn internal_set_error_propagates() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);
        let _intercept = bus.dynamic().on_waterfall("internal/set", |_event, _next| {
            Box::pin(async { Err(EventError::Listener("set boom".into())) })
        });

        assert!(matches!(
            ctx.provide_named_async::<Counter>("counter".to_owned(), Arc::new(Counter(5)))
                .await,
            Err(ContextError::Event(_))
        ));
    }

    #[tokio::test]
    async fn internal_get_error_propagates() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let ctx = ctx.with_event_bus(&bus);
        let _h = ctx.provide::<Counter>(Arc::new(Counter(1))).unwrap();
        let _intercept = bus.dynamic().on_waterfall("internal/get", |_event, _next| {
            Box::pin(async { Err(EventError::Listener("get boom".into())) })
        });

        assert!(matches!(
            ctx.require_named_async::<Counter>("counter").await,
            Err(ContextError::Event(_))
        ));
    }

    #[test]
    fn tag_round_trip_and_propagation() {
        let ctx = Context::new();
        assert!(ctx.tag().is_none());

        let tagged = ctx.tagged(Arc::from("agent-a"));
        assert_eq!(tagged.tag().map(|tag| tag.as_ref()), Some("agent-a"));
        assert_eq!(
            tagged.with_filter(|_| true).tag().map(|tag| tag.as_ref()),
            Some("agent-a")
        );
        assert_eq!(
            tagged.isolate("tools").tag().map(|tag| tag.as_ref()),
            Some("agent-a")
        );
        assert_eq!(
            tagged
                .scoped(std::sync::Weak::new())
                .tag()
                .map(|tag| tag.as_ref()),
            Some("agent-a")
        );
        assert_eq!(
            tagged
                .intercept("llm", Value::new(()))
                .tag()
                .map(|tag| tag.as_ref()),
            Some("agent-a")
        );

        // The original context is untouched.
        assert!(ctx.tag().is_none());
    }

    #[tokio::test]
    async fn effect_and_dispose_effects_run_in_order() {
        let ctx = Context::new();
        let order = Arc::new(Mutex::new(Vec::<usize>::new()));
        let o1 = Arc::clone(&order);
        let _ = ctx.effect(move || o1.lock().unwrap().push(1));
        let o2 = Arc::clone(&order);
        let _ = ctx.effect(move || o2.lock().unwrap().push(2));

        ctx.dispose_effects().await;
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
        // Disposal is drain-and-clear: a second call is a no-op.
        ctx.dispose_effects().await;
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
    }

    #[tokio::test]
    async fn tagged_branch_has_own_effects() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let root = Context::new();
        let root_ran = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&root_ran);
        let _ = root.effect(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });

        let branch = root.tagged(Arc::from("agent-a"));
        let branch_ran = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&branch_ran);
        let _ = branch.effect(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });

        // Disposing the branch must not run root effects.
        branch.dispose_effects().await;
        assert_eq!(branch_ran.load(Ordering::SeqCst), 1);
        assert_eq!(root_ran.load(Ordering::SeqCst), 0);

        root.dispose_effects().await;
        assert_eq!(root_ran.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn scoped_effect_registrations_are_caller_fiber_owned() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let bus = EventBus::new();
        let root = Context::new().with_event_bus(&bus);
        let fiber = Fiber::new();
        let scoped = root.scoped(Arc::downgrade(&fiber));

        // Registrations through the scoped context (cleanups and listeners
        // alike) are adopted by the caller fiber, not only by the branch.
        let ran = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ran);
        let _ = scoped.effect(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let heard = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&heard);
        let _listener = scoped.on::<Ping, _>(move |_event| {
            let counter = Arc::clone(&counter);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Value::new(()))
            })
        });

        bus.emit(Arc::new(Ping));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(heard.load(Ordering::SeqCst), 1);

        // Disposing the caller fiber runs its cleanups and removes its
        // listeners; a later emit no longer reaches the scoped listener.
        fiber.dispose().await;
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        bus.emit(Arc::new(Ping));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(heard.load(Ordering::SeqCst), 1);

        // Root-branch effects are untouched by the fiber's disposal.
        let root_ran = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&root_ran);
        let _ = root.effect(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(root_ran.load(Ordering::SeqCst), 0);
    }
}
