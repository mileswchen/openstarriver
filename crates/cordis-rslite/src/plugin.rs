use std::any::Any;
use std::collections::{HashMap, HashSet, VecDeque};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;

use crate::context::{Context, Spawner};
use crate::effect::EffectHandle;
use crate::events::{EventBus, EventError};
use crate::fiber::{Fiber, FiberState};
use crate::service::TypedService;
use crate::value::{Config, Value};

/// Stable identity assigned to every running plugin instance.
///
/// Each [`Context::plugin`](crate::Context::plugin) call allocates a fresh key,
/// even when the same plugin type is started more than once. Keys can be used
/// to remove or replace a specific instance without relying on display names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PluginKey(u64);

impl PluginKey {
    /// Construct a user-defined key from a raw number.
    ///
    /// Prefer keys produced by the registry for new instances; `from_raw`
    /// exists for persistence and test fixtures.
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }

    /// Expose the numeric key for persistence or diagnostics.
    pub const fn as_raw(self) -> u64 {
        self.0
    }
}

static NEXT_PLUGIN: AtomicU64 = AtomicU64::new(0);

/// One required service and its optional intercept configuration.
///
/// A dependency declared through [`Inject::require_with`] or
/// [`Inject::require_with_value`] also contributes an intercept config to the
/// plugin's context, mirroring Cordis's `Inject` object form where a service
/// name maps to optional intercept config.
#[derive(Debug, Clone)]
pub struct Dependency {
    /// Required service name.
    pub name: String,
    /// Config appended to that service's intercept chain in the plugin context.
    pub config: Option<Config>,
}

/// Normalized plugin service dependencies.
///
/// Entries must be satisfiable by a service registered with `provide` /
/// `provide_named` / `provide_any`. The optional per-dependency intercept
/// config is applied to the plugin's scoped context before its `apply` runs.
#[derive(Debug, Clone, Default)]
pub struct Inject {
    entries: Vec<Dependency>,
}

impl Inject {
    /// Construct an inject declaration from service names.
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            entries: names
                .into_iter()
                .map(|name| Dependency {
                    name: name.into(),
                    config: None,
                })
                .collect(),
        }
    }

    /// Construct an empty declaration.
    pub fn none() -> Self {
        Self::default()
    }

    /// Add a required service without intercept config.
    pub fn require(mut self, name: impl Into<String>) -> Self {
        self.entries.push(Dependency {
            name: name.into(),
            config: None,
        });
        self
    }

    /// Add a required service and intercept config.
    pub fn require_with<T>(mut self, name: impl Into<String>, config: T) -> Self
    where
        T: Any + Send + Sync,
    {
        self.entries.push(Dependency {
            name: name.into(),
            config: Some(Value::new(config)),
        });
        self
    }

    /// Add a type-erased intercept config.
    pub fn require_with_value(mut self, name: impl Into<String>, config: Config) -> Self {
        self.entries.push(Dependency {
            name: name.into(),
            config: Some(config),
        });
        self
    }

    /// Iterate dependencies in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &Dependency> {
        self.entries.iter()
    }

    /// Number of dependencies.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no services are required.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether this declaration contains `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|entry| entry.name == name)
    }

    /// Return just the service names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.name.as_str())
    }
}

impl<const N: usize> From<[&str; N]> for Inject {
    fn from(value: [&str; N]) -> Self {
        Self::new(value)
    }
}

impl From<Vec<String>> for Inject {
    fn from(value: Vec<String>) -> Self {
        Self::new(value)
    }
}

pub trait Plugin: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    fn inject(&self) -> Inject {
        Inject::none()
    }

    fn provides(&self) -> &[&'static str] {
        &[]
    }

    /// Validate and optionally normalize raw configuration before startup.
    ///
    /// The default accepts any config. Config-aware plugins should override
    /// this to type-check and, when they declare a schema, validate fields.
    /// Errors fail the plugin activation without tearing down an already
    /// running instance.
    fn validate_config(&self, config: Config) -> anyhow::Result<Config> {
        Ok(config)
    }

    /// Apply this plugin without configuration.
    ///
    /// Config-aware plugins should implement [`Plugin::apply_with_config`]
    /// instead; calling this method directly on such a plugin returns an
    /// error. The registry always routes through `apply_with_config`.
    fn apply(
        &self,
        ctx: &Context,
        bus: &EventBus,
    ) -> crate::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let name = self.name();
        let _ = (ctx, bus);
        Box::pin(async move { anyhow::bail!("plugin `{name}` requires config") })
    }

    /// Apply this plugin with validated configuration.
    ///
    /// The default delegates to [`Plugin::apply`], so legacy plugins keep
    /// working unchanged. Config-aware plugins override this method.
    fn apply_with_config(
        &self,
        ctx: &Context,
        bus: &EventBus,
        _config: Config,
    ) -> crate::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        self.apply(ctx, bus)
    }
}

pub struct SyncFnPlugin<F> {
    name: &'static str,
    inject: Inject,
    callback: F,
}

impl<F> Plugin for SyncFnPlugin<F>
where
    F: Fn(&Context, &EventBus) -> anyhow::Result<Vec<EffectHandle>> + Send + Sync + 'static,
{
    fn name(&self) -> &'static str {
        self.name
    }

    fn inject(&self) -> Inject {
        self.inject.clone()
    }

    fn apply(
        &self,
        ctx: &Context,
        bus: &EventBus,
    ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
        let result = (self.callback)(ctx, bus);
        Box::pin(async move { result })
    }
}

pub fn plugin_sync<F>(name: &'static str, inject: Inject, callback: F) -> SyncFnPlugin<F>
where
    F: Fn(&Context, &EventBus) -> anyhow::Result<Vec<EffectHandle>> + Send + Sync + 'static,
{
    SyncFnPlugin {
        name,
        inject,
        callback,
    }
}

pub struct AsyncFnPlugin<F> {
    name: &'static str,
    inject: Inject,
    callback: F,
}

impl<F> Plugin for AsyncFnPlugin<F>
where
    F: Fn(&Context, &EventBus) -> crate::BoxFuture<anyhow::Result<Vec<EffectHandle>>>
        + Send
        + Sync
        + 'static,
{
    fn name(&self) -> &'static str {
        self.name
    }

    fn inject(&self) -> Inject {
        self.inject.clone()
    }

    fn apply(
        &self,
        ctx: &Context,
        bus: &EventBus,
    ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
        (self.callback)(ctx, bus)
    }
}

pub fn plugin_async<F>(name: &'static str, inject: Inject, callback: F) -> AsyncFnPlugin<F>
where
    F: Fn(&Context, &EventBus) -> crate::BoxFuture<anyhow::Result<Vec<EffectHandle>>>
        + Send
        + Sync
        + 'static,
{
    AsyncFnPlugin {
        name,
        inject,
        callback,
    }
}

pub struct SyncServicePlugin<S, F> {
    name: &'static str,
    inject: Inject,
    constructor: F,
    _marker: PhantomData<fn() -> S>,
}

impl<S, F> Plugin for SyncServicePlugin<S, F>
where
    S: TypedService,
    F: Fn(&Context) -> anyhow::Result<S> + Send + Sync + 'static,
{
    fn name(&self) -> &'static str {
        self.name
    }

    fn inject(&self) -> Inject {
        self.inject.clone()
    }

    fn provides(&self) -> &[&'static str] {
        &[S::NAME]
    }

    fn apply(
        &self,
        ctx: &Context,
        _bus: &EventBus,
    ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
        let result = (self.constructor)(ctx).and_then(|service| {
            ctx.provide::<S>(Arc::new(service))
                .map(|handle| vec![handle])
                .map_err(anyhow::Error::from)
        });
        Box::pin(async move { result })
    }
}

pub fn service_sync<S, F>(
    name: &'static str,
    inject: Inject,
    constructor: F,
) -> SyncServicePlugin<S, F>
where
    S: TypedService,
    F: Fn(&Context) -> anyhow::Result<S> + Send + Sync + 'static,
{
    SyncServicePlugin {
        name,
        inject,
        constructor,
        _marker: PhantomData,
    }
}

pub struct AsyncServicePlugin<S, F> {
    name: &'static str,
    inject: Inject,
    constructor: F,
    _marker: PhantomData<fn() -> S>,
}

impl<S, F> Plugin for AsyncServicePlugin<S, F>
where
    S: TypedService,
    F: Fn(&Context) -> crate::BoxFuture<anyhow::Result<S>> + Send + Sync + 'static,
{
    fn name(&self) -> &'static str {
        self.name
    }

    fn inject(&self) -> Inject {
        self.inject.clone()
    }

    fn provides(&self) -> &[&'static str] {
        &[S::NAME]
    }

    fn apply(
        &self,
        ctx: &Context,
        _bus: &EventBus,
    ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
        let fut = (self.constructor)(ctx);
        let ctx = ctx.clone();
        Box::pin(async move {
            let service = fut.await?;
            ctx.provide::<S>(Arc::new(service))
                .map(|handle| vec![handle])
                .map_err(anyhow::Error::from)
        })
    }
}

pub fn service_async<S, F>(
    name: &'static str,
    inject: Inject,
    constructor: F,
) -> AsyncServicePlugin<S, F>
where
    S: TypedService,
    F: Fn(&Context) -> crate::BoxFuture<anyhow::Result<S>> + Send + Sync + 'static,
{
    AsyncServicePlugin {
        name,
        inject,
        constructor,
        _marker: PhantomData,
    }
}

pub struct SyncFnConfigPlugin<C, F> {
    name: &'static str,
    inject: Inject,
    callback: F,
    _marker: PhantomData<fn() -> C>,
}

impl<C, F> Plugin for SyncFnConfigPlugin<C, F>
where
    C: Send + Sync + 'static,
    F: Fn(&Context, &EventBus, Arc<C>) -> anyhow::Result<Vec<EffectHandle>> + Send + Sync + 'static,
{
    fn name(&self) -> &'static str {
        self.name
    }

    fn inject(&self) -> Inject {
        self.inject.clone()
    }

    fn validate_config(&self, config: Config) -> anyhow::Result<Config> {
        config
            .downcast::<C>()
            .map_err(|error| anyhow::anyhow!("invalid config for `{}`: {error}", self.name))?;
        Ok(config)
    }

    fn apply_with_config(
        &self,
        ctx: &Context,
        bus: &EventBus,
        config: Config,
    ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
        let config = match config.downcast::<C>() {
            Ok(config) => config,
            Err(error) => {
                let name = self.name;
                return Box::pin(async move {
                    Err(anyhow::anyhow!("invalid config for `{name}`: {error}"))
                });
            }
        };
        let result = (self.callback)(ctx, bus, config);
        Box::pin(async move { result })
    }
}

pub fn plugin_sync_with_config<C, F>(
    name: &'static str,
    inject: Inject,
    callback: F,
) -> SyncFnConfigPlugin<C, F>
where
    C: Send + Sync + 'static,
    F: Fn(&Context, &EventBus, Arc<C>) -> anyhow::Result<Vec<EffectHandle>> + Send + Sync + 'static,
{
    SyncFnConfigPlugin {
        name,
        inject,
        callback,
        _marker: PhantomData,
    }
}

pub struct AsyncFnConfigPlugin<C, F> {
    name: &'static str,
    inject: Inject,
    callback: F,
    _marker: PhantomData<fn() -> C>,
}

impl<C, F> Plugin for AsyncFnConfigPlugin<C, F>
where
    C: Send + Sync + 'static,
    F: Fn(&Context, &EventBus, Arc<C>) -> crate::BoxFuture<anyhow::Result<Vec<EffectHandle>>>
        + Send
        + Sync
        + 'static,
{
    fn name(&self) -> &'static str {
        self.name
    }

    fn inject(&self) -> Inject {
        self.inject.clone()
    }

    fn validate_config(&self, config: Config) -> anyhow::Result<Config> {
        config
            .downcast::<C>()
            .map_err(|error| anyhow::anyhow!("invalid config for `{}`: {error}", self.name))?;
        Ok(config)
    }

    fn apply_with_config(
        &self,
        ctx: &Context,
        bus: &EventBus,
        config: Config,
    ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
        let config = match config.downcast::<C>() {
            Ok(config) => config,
            Err(error) => {
                let name = self.name;
                return Box::pin(async move {
                    Err(anyhow::anyhow!("invalid config for `{name}`: {error}"))
                });
            }
        };
        (self.callback)(ctx, bus, config)
    }
}

pub fn plugin_async_with_config<C, F>(
    name: &'static str,
    inject: Inject,
    callback: F,
) -> AsyncFnConfigPlugin<C, F>
where
    C: Send + Sync + 'static,
    F: Fn(&Context, &EventBus, Arc<C>) -> crate::BoxFuture<anyhow::Result<Vec<EffectHandle>>>
        + Send
        + Sync
        + 'static,
{
    AsyncFnConfigPlugin {
        name,
        inject,
        callback,
        _marker: PhantomData,
    }
}

pub struct SyncServiceConfigPlugin<S, C, F> {
    name: &'static str,
    inject: Inject,
    constructor: F,
    _marker: PhantomData<fn() -> (S, C)>,
}

impl<S, C, F> Plugin for SyncServiceConfigPlugin<S, C, F>
where
    S: TypedService,
    C: Send + Sync + 'static,
    F: Fn(&Context, Arc<C>) -> anyhow::Result<S> + Send + Sync + 'static,
{
    fn name(&self) -> &'static str {
        self.name
    }

    fn inject(&self) -> Inject {
        self.inject.clone()
    }

    fn provides(&self) -> &[&'static str] {
        &[S::NAME]
    }

    fn validate_config(&self, config: Config) -> anyhow::Result<Config> {
        config
            .downcast::<C>()
            .map_err(|error| anyhow::anyhow!("invalid config for `{}`: {error}", self.name))?;
        Ok(config)
    }

    fn apply_with_config(
        &self,
        ctx: &Context,
        _bus: &EventBus,
        config: Config,
    ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
        let config = match config.downcast::<C>() {
            Ok(config) => config,
            Err(error) => {
                let name = self.name;
                return Box::pin(async move {
                    Err(anyhow::anyhow!("invalid config for `{name}`: {error}"))
                });
            }
        };
        let result = (self.constructor)(ctx, config).and_then(|service| {
            ctx.provide::<S>(Arc::new(service))
                .map(|handle| vec![handle])
                .map_err(anyhow::Error::from)
        });
        Box::pin(async move { result })
    }
}

pub fn service_sync_with_config<S, C, F>(
    name: &'static str,
    inject: Inject,
    constructor: F,
) -> SyncServiceConfigPlugin<S, C, F>
where
    S: TypedService,
    C: Send + Sync + 'static,
    F: Fn(&Context, Arc<C>) -> anyhow::Result<S> + Send + Sync + 'static,
{
    SyncServiceConfigPlugin {
        name,
        inject,
        constructor,
        _marker: PhantomData,
    }
}

pub struct AsyncServiceConfigPlugin<S, C, F> {
    name: &'static str,
    inject: Inject,
    constructor: F,
    _marker: PhantomData<fn() -> (S, C)>,
}

impl<S, C, F> Plugin for AsyncServiceConfigPlugin<S, C, F>
where
    S: TypedService,
    C: Send + Sync + 'static,
    F: Fn(&Context, Arc<C>) -> crate::BoxFuture<anyhow::Result<S>> + Send + Sync + 'static,
{
    fn name(&self) -> &'static str {
        self.name
    }

    fn inject(&self) -> Inject {
        self.inject.clone()
    }

    fn provides(&self) -> &[&'static str] {
        &[S::NAME]
    }

    fn validate_config(&self, config: Config) -> anyhow::Result<Config> {
        config
            .downcast::<C>()
            .map_err(|error| anyhow::anyhow!("invalid config for `{}`: {error}", self.name))?;
        Ok(config)
    }

    fn apply_with_config(
        &self,
        ctx: &Context,
        _bus: &EventBus,
        config: Config,
    ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
        let config = match config.downcast::<C>() {
            Ok(config) => config,
            Err(error) => {
                let name = self.name;
                return Box::pin(async move {
                    Err(anyhow::anyhow!("invalid config for `{name}`: {error}"))
                });
            }
        };
        let ctx = ctx.clone();
        let fut = (self.constructor)(&ctx, config);
        Box::pin(async move {
            let service = fut.await?;
            ctx.provide::<S>(Arc::new(service))
                .map(|handle| vec![handle])
                .map_err(anyhow::Error::from)
        })
    }
}

pub fn service_async_with_config<S, C, F>(
    name: &'static str,
    inject: Inject,
    constructor: F,
) -> AsyncServiceConfigPlugin<S, C, F>
where
    S: TypedService,
    C: Send + Sync + 'static,
    F: Fn(&Context, Arc<C>) -> crate::BoxFuture<anyhow::Result<S>> + Send + Sync + 'static,
{
    AsyncServiceConfigPlugin {
        name,
        inject,
        constructor,
        _marker: PhantomData,
    }
}

struct PluginEntry {
    plugin: Arc<dyn Plugin>,
    fiber: Arc<Fiber>,
    inject: Inject,
    key: PluginKey,
    raw_config: Mutex<Config>,
    validated_config: Mutex<Option<Config>>,
    config_generation: AtomicU64,
    active_epoch: Mutex<Option<Vec<u64>>>,
}

#[derive(Clone)]
struct PluginRegistration {
    plugin: Arc<dyn Plugin>,
    config: Config,
}

/// Default cap on total reconcile executions inside one worker run (one
/// "wave") before the worker assumes a dynamic dependency cycle, warns, and
/// abandons the wave. Legitimate graphs converge in roughly one reconcile per
/// plugin per wave, so 64 comfortably covers normal boot cascades while
/// bounding a thrashing graph. Tunable via
/// [`PluginRegistry::set_reconcile_iteration_caps`].
const DEFAULT_MAX_RECONCILE_ITERATIONS: usize = 64;

/// Default cap on how many times a single plugin may be reconciled inside one
/// worker run before the wave is abandoned. The pending queue dedupes within
/// a drain pass, so a healthy plugin reconciles only a handful of times per
/// wave even when several of its dependencies change mid-wave.
const DEFAULT_MAX_PLUGIN_RECONCILES: usize = 16;

pub(crate) struct RegistryState {
    ctx: Context,
    bus: EventBus,
    entries: Mutex<HashMap<PluginKey, Arc<PluginEntry>>>,
    dependents: Mutex<HashMap<String, Vec<PluginKey>>>,
    pending: Mutex<VecDeque<PluginKey>>,
    queued: Mutex<HashSet<PluginKey>>,
    reconciling: AtomicBool,
    dirty: AtomicBool,
    scheduled: AtomicBool,
    apply_timeout: Option<Duration>,
    max_reconcile_iterations: usize,
    max_plugin_reconciles: usize,
}

impl RegistryState {
    fn enqueue_entry(&self, key: PluginKey) {
        let mut queued = self.queued.lock().expect("queued poisoned");
        if queued.insert(key) {
            self.pending
                .lock()
                .expect("pending poisoned")
                .push_back(key);
        }
    }

    fn enqueue_dependents(&self, name: &str) {
        if let Some(keys) = self
            .dependents
            .lock()
            .expect("dependents poisoned")
            .get(name)
        {
            for &key in keys {
                self.enqueue_entry(key);
            }
        }
    }

    fn pop_pending(&self) -> Option<PluginKey> {
        let key = self.pending.lock().expect("pending poisoned").pop_front()?;
        self.queued.lock().expect("queued poisoned").remove(&key);
        Some(key)
    }

    fn spawner(self: &Arc<Self>) -> Spawner {
        let state = Arc::clone(self);
        Arc::new(move |child, parent, config| {
            let state = Arc::clone(&state);
            Box::pin(async move { state.spawn_child(child, parent, config).await })
        })
    }

    fn spawn_child(
        self: &Arc<Self>,
        child: Arc<dyn Plugin>,
        parent: Arc<Fiber>,
        config: Config,
    ) -> crate::BoxFuture<anyhow::Result<PluginHandle>> {
        self.spawn_plugin(child, Some(parent), config)
    }

    /// Start a plugin instance as either a top-level runtime entry or a child
    /// of `parent`. When `parent` is present, disposing the parent also
    /// disposes this instance, matching `ctx.plugin()` parent ownership.
    pub(crate) fn spawn_plugin(
        self: &Arc<Self>,
        child: Arc<dyn Plugin>,
        parent: Option<Arc<Fiber>>,
        config: Config,
    ) -> crate::BoxFuture<anyhow::Result<PluginHandle>> {
        let state = Arc::clone(self);
        Box::pin(async move {
            let inject = child.inject();
            let fiber = Fiber::new();
            let key = PluginKey(NEXT_PLUGIN.fetch_add(1, Ordering::Relaxed) + 1);
            let entry = Arc::new(PluginEntry {
                plugin: child,
                fiber: Arc::clone(&fiber),
                inject,
                key,
                raw_config: Mutex::new(config),
                validated_config: Mutex::new(None),
                config_generation: AtomicU64::new(0),
                active_epoch: Mutex::new(None),
            });
            {
                let mut entries = state.entries.lock().expect("entries poisoned");
                entries.insert(key, Arc::clone(&entry));
            }
            {
                let mut dependents = state.dependents.lock().expect("dependents poisoned");
                for dep in entry.plugin.inject().names() {
                    dependents.entry(dep.to_owned()).or_default().push(key);
                }
            }

            // parent-effect: disposing the parent disposes this child.
            // adopt REJECTS (and drops the closure) on a Disposing/Disposed
            // parent — the child would outlive it forever, so dispose it now.
            if let Some(parent) = parent {
                let child_fiber = Arc::clone(&fiber);
                if parent
                    .adopt(Box::new(move || {
                        Box::pin(async move { child_fiber.dispose().await })
                    }))
                    .is_err()
                {
                    fiber.dispose().await;
                    return Err(anyhow::anyhow!(
                        "parent fiber was disposed while spawning a child plugin"
                    ));
                }
            }

            state.enqueue_entry(key);
            if let Err(err) = state.run_worker().await {
                fiber.dispose().await;
                return Err(err);
            }

            Ok(PluginHandle {
                entry: Arc::clone(&entry),
                _state: Arc::clone(&state),
            })
        })
    }

    fn request_reconcile(self: &Arc<Self>, name: &str) {
        self.enqueue_dependents(name);
        self.dirty.store(true, Ordering::SeqCst);
        if self.scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let _ = state.run_worker().await;
            state.scheduled.store(false, Ordering::SeqCst);
            if state.dirty.load(Ordering::SeqCst) {
                state.schedule_worker();
            }
        });
    }

    fn schedule_worker(self: &Arc<Self>) {
        if self.scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let _ = state.run_worker().await;
            state.scheduled.store(false, Ordering::SeqCst);
            if state.dirty.load(Ordering::SeqCst) {
                state.schedule_worker();
            }
        });
    }

    async fn run_worker(self: &Arc<Self>) -> anyhow::Result<()> {
        if self.reconciling.swap(true, Ordering::SeqCst) {
            self.dirty.store(true, Ordering::SeqCst);
            return Ok(());
        }
        let _guard = ReconcilingGuard(Arc::clone(self));
        let max_iterations = self.max_reconcile_iterations;
        let max_per_plugin = self.max_plugin_reconciles;
        let mut iterations = 0usize;
        let mut per_plugin: HashMap<PluginKey, usize> = HashMap::new();
        let mut first_error: Option<anyhow::Error> = None;
        loop {
            self.dirty.store(false, Ordering::SeqCst);
            while let Some(key) = self.pop_pending() {
                let entry = {
                    let entries = self.entries.lock().expect("entries poisoned");
                    let Some(entry) = entries.get(&key) else {
                        continue;
                    };
                    Arc::clone(entry)
                };
                iterations += 1;
                let plugin_reconciles = {
                    let count = per_plugin.entry(key).or_insert(0);
                    *count += 1;
                    *count
                };
                if iterations > max_iterations || plugin_reconciles > max_per_plugin {
                    tracing::warn!(
                        plugin = entry.plugin.name(),
                        plugin_key = key.as_raw(),
                        wave_iterations = iterations,
                        plugin_reconciles,
                        max_reconcile_iterations = max_iterations,
                        max_plugin_reconciles = max_per_plugin,
                        "reconcile iteration cap exceeded; abandoning this wave to break a suspected dynamic dependency cycle"
                    );
                    // Defensive brake (spec §18): a dynamic cycle that static
                    // detection cannot see would spin this loop forever. Give
                    // up on the current wave without panicking; the next
                    // service notification starts a fresh wave with a full
                    // iteration budget.
                    self.abandon_wave();
                    return Ok(());
                }
                let before = self.ctx.provided_names(&entry.fiber);
                // A failing plugin must not abort the wave — every entry
                // still queued would starve until an unrelated notification.
                match self.reconcile(&entry).await {
                    Ok(true) => {
                        let after = self.ctx.provided_names(&entry.fiber);
                        for name in before.union(&after) {
                            self.enqueue_dependents(name);
                        }
                    }
                    Ok(false) => {}
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                        continue;
                    }
                }
            }
            if !self.dirty.load(Ordering::SeqCst) {
                break;
            }
        }
        // Surface the first plugin failure AFTER draining the queue — boot
        // and update keep their Err, siblings no longer starve.
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Drop everything still queued for reconcile and clear the dirty flag,
    /// ending the current wave. Entries keep their `active_epoch`, so any
    /// plugin left mid-transition is re-reconciled by the next service
    /// notification for its dependencies.
    fn abandon_wave(&self) {
        {
            let mut pending = self.pending.lock().expect("pending poisoned");
            pending.clear();
        }
        // Clear `pending` before `queued`: an enqueue interleaved between the
        // two clears then leaves the key in `pending` (processed next wave)
        // rather than stuck in `queued` (never enqueued again).
        self.queued.lock().expect("queued poisoned").clear();
        self.dirty.store(false, Ordering::SeqCst);
    }

    async fn reconcile(self: &Arc<Self>, entry: &Arc<PluginEntry>) -> anyhow::Result<bool> {
        // A shut-down plugin's fiber is Disposed: set_pending/set_loading are
        // no-ops from there, and re-applying would resurrect its effects on
        // the ROOT context (permanently live after the entry is gone).
        if (entry.fiber.state() as u8) >= crate::fiber::FiberState::Disposing as u8 {
            return Ok(false);
        }
        let inject_refs: Vec<&str> = entry.inject.names().collect();
        let desired = self.ctx.dependency_epoch(&inject_refs).map(|mut epoch| {
            epoch.push(entry.config_generation.load(Ordering::SeqCst));
            epoch
        });
        let active = entry.active_epoch.lock().expect("epoch poisoned").clone();

        match desired {
            Some(epoch) => {
                if active.as_ref() == Some(&epoch) {
                    return Ok(false);
                }
                let was_active = active.is_some();
                if was_active {
                    // Release the previous incarnation's effects before
                    // re-applying. Re-provided services otherwise collide with
                    // the old implementation still occupying the same scope.
                    entry.fiber.clear_effects().await;
                    entry.fiber.set_pending();
                }
                entry.fiber.set_loading();
                let mut scoped_ctx = self
                    .ctx
                    .scoped(Arc::downgrade(&entry.fiber))
                    .with_spawner(self.spawner());
                for dep in entry.inject.iter() {
                    if let Some(config) = dep.config.clone() {
                        scoped_ctx = scoped_ctx.intercept(dep.name.clone(), config);
                    }
                }
                let entry_for_apply = Arc::clone(entry);
                let scoped_for_apply = scoped_ctx.clone();
                let bus_for_apply = self.bus.clone();
                let raw_for_apply = entry
                    .raw_config
                    .lock()
                    .expect("raw config poisoned")
                    .clone();
                let apply_fut = async move {
                    let config = resolve_config(
                        &entry_for_apply.plugin,
                        raw_for_apply,
                        &scoped_for_apply,
                        &bus_for_apply,
                    )
                    .await?;
                    *entry_for_apply
                        .validated_config
                        .lock()
                        .expect("validated config poisoned") = Some(config.clone());
                    entry_for_apply
                        .plugin
                        .apply_with_config(&scoped_for_apply, &bus_for_apply, config)
                        .await
                };
                let result = match self.apply_timeout {
                    Some(timeout) => match tokio::time::timeout(timeout, apply_fut).await {
                        Ok(result) => result,
                        Err(_) => Err(anyhow::anyhow!(
                            "plugin `{}` apply timed out",
                            entry.plugin.name()
                        )),
                    },
                    None => apply_fut.await,
                };
                match result {
                    Ok(effects) => {
                        let mut adopted = true;
                        for effect in effects {
                            if entry.fiber.adopt_effect(effect).is_err() {
                                // The fiber reached Disposing/Disposed
                                // mid-apply — a benign race. Do the same
                                // bookkeeping as the Err arm (no stale
                                // active_epoch) instead of failing the wave.
                                adopted = false;
                                break;
                            }
                        }
                        if !adopted {
                            entry.fiber.clear_effects().await;
                            *entry.active_epoch.lock().expect("epoch poisoned") = None;
                            entry.fiber.set_failed();
                            return Ok(false);
                        }
                        *entry.active_epoch.lock().expect("epoch poisoned") = Some(epoch);
                        entry.fiber.set_active();
                        Ok(true)
                    }
                    Err(err) => {
                        // Drop any effects the failed apply auto-adopted so a
                        // later retry starts from a clean slot.
                        entry.fiber.clear_effects().await;
                        *entry.active_epoch.lock().expect("epoch poisoned") = None;
                        entry.fiber.set_failed();
                        tracing::warn!(plugin = entry.plugin.name(), "activation failed: {err:#}");
                        Err(err)
                    }
                }
            }
            None => {
                if active.is_some() {
                    entry.fiber.clear_effects().await;
                    *entry.active_epoch.lock().expect("epoch poisoned") = None;
                    entry.fiber.set_pending();
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }

    async fn update(
        self: &Arc<Self>,
        entry: Arc<PluginEntry>,
        config: Config,
    ) -> anyhow::Result<()> {
        let scoped = self.ctx.scoped(Arc::downgrade(&entry.fiber));
        let args = vec![Value::new(scoped), config.clone()];
        let config_for_inner = config.clone();
        let inner = move || -> crate::BoxFuture<Result<Option<Value>, EventError>> {
            let config = config_for_inner.clone();
            Box::pin(async move { Ok(Some(config)) })
        };
        let outcome = self
            .bus
            .dynamic()
            .waterfall("internal/update", args, inner)
            .await?;
        let new_config = outcome.unwrap_or(config);

        {
            let mut raw = entry.raw_config.lock().expect("raw config poisoned");
            *raw = new_config;
        }
        entry.config_generation.fetch_add(1, Ordering::SeqCst);

        let registered = {
            let entries = self.entries.lock().expect("entries poisoned");
            entries.contains_key(&entry.key)
        };
        if registered {
            self.enqueue_entry(entry.key);
            self.dirty.store(true, Ordering::SeqCst);
            self.run_worker().await?;
        }
        Ok(())
    }

    /// Dispose the plugin instance identified by `key` and stop reconciling it.
    ///
    /// The entry is physically removed from the registry, mirroring Cordis's
    /// runtime map deletion. Its fiber is disposed, which also drops every
    /// service it provided, and dependent plugins re-evaluate their own
    /// dependencies.
    pub(crate) async fn remove_plugin(self: &Arc<Self>, key: PluginKey) -> bool {
        let entry = {
            let entries = self.entries.lock().expect("entries poisoned");
            let Some(entry) = entries.get(&key) else {
                return false;
            };
            Arc::clone(entry)
        };

        {
            let mut entries = self.entries.lock().expect("entries poisoned");
            entries.remove(&key);
        }

        // Stop routing future service notifications to this entry.
        {
            let mut dependents = self.dependents.lock().expect("dependents poisoned");
            for dep in entry.plugin.inject().names() {
                if let Some(keys) = dependents.get_mut(dep) {
                    keys.retain(|&other| other != key);
                }
            }
            dependents.retain(|_, keys| !keys.is_empty());
        }

        // Drop it from the pending queue so a removed entry is never reconciled.
        {
            let mut queued = self.queued.lock().expect("queued poisoned");
            if queued.remove(&key) {
                let mut pending = self.pending.lock().expect("pending poisoned");
                pending.retain(|&other| other != key);
            }
        }

        entry.fiber.dispose().await;
        true
    }
}

async fn resolve_config(
    plugin: &Arc<dyn Plugin>,
    raw: Config,
    ctx: &Context,
    bus: &EventBus,
) -> anyhow::Result<Config> {
    let name = plugin.name().to_owned();
    let args = vec![Value::new(ctx.clone()), raw.clone()];
    let raw_for_inner = raw.clone();
    let inner = move || -> crate::BoxFuture<Result<Option<Value>, EventError>> {
        let raw = raw_for_inner.clone();
        Box::pin(async move { Ok(Some(raw)) })
    };
    let outcome = bus
        .dynamic()
        .waterfall("internal/config", args, inner)
        .await?;
    let config = outcome.unwrap_or(raw);
    plugin
        .validate_config(config)
        .map_err(|error| anyhow::anyhow!("plugin `{name}` invalid config: {error:#}"))
}

struct ReconcilingGuard(Arc<RegistryState>);

impl Drop for ReconcilingGuard {
    fn drop(&mut self) {
        self.0.reconciling.store(false, Ordering::SeqCst);
    }
}

pub struct PluginHandle {
    entry: Arc<PluginEntry>,
    _state: Arc<RegistryState>,
}

impl PluginHandle {
    pub fn name(&self) -> &'static str {
        self.entry.plugin.name()
    }

    /// Return the stable identity of this running instance.
    pub fn key(&self) -> PluginKey {
        self.entry.key
    }

    pub fn state(&self) -> FiberState {
        self.entry.fiber.state()
    }

    pub async fn shutdown(&self) {
        self.entry.fiber.dispose().await;
    }

    /// Validate and apply new configuration, then restart the plugin.
    pub async fn update(&self, config: Config) -> anyhow::Result<()> {
        let registered = self
            ._state
            .entries
            .lock()
            .expect("entries poisoned")
            .contains_key(&self.entry.key);
        if !registered {
            anyhow::bail!("plugin `{}` has been removed", self.entry.plugin.name());
        }
        self._state.update(Arc::clone(&self.entry), config).await
    }
}

pub struct PluginRegistry {
    plugins: Mutex<Vec<PluginRegistration>>,
    subscription: Mutex<Option<EffectHandle>>,
    apply_timeout: Mutex<Option<Duration>>,
    reconcile_caps: Mutex<Option<(usize, usize)>>,
    runtime: Mutex<Option<Arc<RegistryState>>>,
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self {
            plugins: Mutex::new(Vec::new()),
            subscription: Mutex::new(None),
            apply_timeout: Mutex::new(None),
            reconcile_caps: Mutex::new(None),
            runtime: Mutex::new(None),
        }
    }
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, plugin: impl Plugin) {
        self.register_with_config(plugin, Config::new(()));
    }

    pub fn register_with_config(&self, plugin: impl Plugin, config: Config) {
        self.plugins
            .lock()
            .expect("plugin list poisoned")
            .push(PluginRegistration {
                plugin: Arc::new(plugin),
                config,
            });
    }

    pub fn set_apply_timeout(&self, timeout: Duration) {
        *self.apply_timeout.lock().expect("apply timeout poisoned") = Some(timeout);
    }

    /// Override the defensive reconcile iteration caps for the next
    /// [`PluginRegistry::boot`].
    ///
    /// `max_iterations` bounds the total reconcile executions inside one
    /// worker run (wave); `max_plugin_reconciles` bounds how many times a
    /// single plugin may be reconciled inside one wave. When either cap is
    /// exceeded the worker warns (with the plugin name, its key, and both
    /// counts), abandons the remaining queue, and returns — a suspected
    /// dynamic dependency cycle brakes instead of spinning forever. The next
    /// service notification starts a fresh wave with a full budget.
    ///
    /// Defaults are 64 per wave and 16 per plugin. Raise the wave cap for
    /// plugin graphs whose boot cascade legitimately reconciles more than 64
    /// plugins in a single wave; a braked boot leaves the unprocessed plugins
    /// `Pending` until their next dependency notification.
    pub fn set_reconcile_iteration_caps(
        &self,
        max_iterations: usize,
        max_plugin_reconciles: usize,
    ) {
        *self.reconcile_caps.lock().expect("reconcile caps poisoned") =
            Some((max_iterations, max_plugin_reconciles));
    }

    pub async fn boot(&self, ctx: &Context, bus: &EventBus) -> anyhow::Result<Vec<PluginHandle>> {
        let plugins: Vec<PluginRegistration> = {
            self.plugins
                .lock()
                .expect("plugin list poisoned")
                .iter()
                .cloned()
                .collect()
        };
        detect_cycle(&plugins, ctx)?;

        let apply_timeout = *self.apply_timeout.lock().expect("apply timeout poisoned");
        let (max_reconcile_iterations, max_plugin_reconciles) = {
            self.reconcile_caps
                .lock()
                .expect("reconcile caps poisoned")
                .unwrap_or((
                    DEFAULT_MAX_RECONCILE_ITERATIONS,
                    DEFAULT_MAX_PLUGIN_RECONCILES,
                ))
        };

        let mut top_level: Vec<PluginKey> = Vec::with_capacity(plugins.len());
        let mut dependents: HashMap<String, Vec<PluginKey>> = HashMap::new();
        for plugin in &plugins {
            let key = PluginKey(NEXT_PLUGIN.fetch_add(1, Ordering::Relaxed) + 1);
            top_level.push(key);
            for dep in plugin.plugin.inject().names() {
                dependents.entry(dep.to_owned()).or_default().push(key);
            }
        }

        let state = Arc::new_cyclic(|weak| {
            let runtime_ctx = ctx.with_event_bus(bus);
            runtime_ctx.set_registry(weak.clone());
            let entries = plugins
                .iter()
                .zip(&top_level)
                .map(|(p, &key)| {
                    (
                        key,
                        Arc::new(PluginEntry {
                            plugin: Arc::clone(&p.plugin),
                            fiber: Fiber::new(),
                            inject: p.plugin.inject(),
                            key,
                            raw_config: Mutex::new(p.config.clone()),
                            validated_config: Mutex::new(None),
                            config_generation: AtomicU64::new(0),
                            active_epoch: Mutex::new(None),
                        }),
                    )
                })
                .collect::<HashMap<_, _>>();
            RegistryState {
                ctx: runtime_ctx,
                bus: bus.clone(),
                entries: Mutex::new(entries),
                dependents: Mutex::new(dependents),
                pending: Mutex::new(VecDeque::new()),
                queued: Mutex::new(HashSet::new()),
                reconciling: AtomicBool::new(false),
                dirty: AtomicBool::new(false),
                scheduled: AtomicBool::new(false),
                apply_timeout,
                max_reconcile_iterations,
                max_plugin_reconciles,
            }
        });

        let weak = Arc::downgrade(&state);
        let subscription = ctx.subscribe(Arc::new(move |name| {
            if let Some(state) = weak.upgrade() {
                state.request_reconcile(name);
            }
        }));
        *self.subscription.lock().expect("subscription poisoned") = Some(subscription);
        *self.runtime.lock().expect("runtime poisoned") = Some(Arc::clone(&state));

        for &key in &top_level {
            state.enqueue_entry(key);
        }
        if let Err(err) = state.run_worker().await {
            let fibers: Vec<Arc<Fiber>> = {
                let entries = state.entries.lock().expect("entries poisoned");
                entries.values().map(|e| Arc::clone(&e.fiber)).collect()
            };
            for fiber in fibers {
                fiber.dispose().await;
            }
            return Err(err);
        }

        let entries = state.entries.lock().expect("entries poisoned");
        Ok(top_level
            .into_iter()
            .map(|key| PluginHandle {
                entry: Arc::clone(&entries[&key]),
                _state: Arc::clone(&state),
            })
            .collect())
    }
}

fn detect_cycle(plugins: &[PluginRegistration], ctx: &Context) -> anyhow::Result<()> {
    let mut providers: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
    for plugin in plugins {
        for service in plugin.plugin.provides() {
            providers
                .entry(service)
                .or_default()
                .push(plugin.plugin.name());
        }
    }

    let mut edges: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
    for plugin in plugins {
        for dependency in plugin.plugin.inject().names() {
            if ctx.has(dependency) {
                continue;
            }
            match providers.get(dependency) {
                Some(provider_names) => {
                    let targets = edges.entry(plugin.plugin.name()).or_default();
                    for provider in provider_names {
                        targets.push(provider);
                    }
                }
                None => {
                    tracing::warn!(
                        plugin = plugin.plugin.name(),
                        "injecting service `{}` with no static provider; stays pending until provided",
                        dependency
                    );
                }
            }
        }
    }

    fn visit(
        node: &'static str,
        edges: &HashMap<&'static str, Vec<&'static str>>,
        color: &mut HashMap<&'static str, u8>,
        path: &mut Vec<&'static str>,
    ) -> anyhow::Result<()> {
        color.insert(node, 1);
        path.push(node);
        if let Some(next) = edges.get(node) {
            for &target in next {
                match color.get(target).copied().unwrap_or(0) {
                    0 => visit(target, edges, color, path)?,
                    1 => {
                        path.push(target);
                        anyhow::bail!("dependency cycle detected: {:?}", path);
                    }
                    _ => {}
                }
            }
        }
        path.pop();
        color.insert(node, 2);
        Ok(())
    }

    let mut color: HashMap<&'static str, u8> = HashMap::new();
    for plugin in plugins {
        if color.get(plugin.plugin.name()).copied().unwrap_or(0) == 0 {
            let mut path = Vec::new();
            visit(plugin.plugin.name(), &edges, &mut color, &mut path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::TypedService;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Greeting(Arc<str>);
    impl TypedService for Greeting {
        const NAME: &'static str = "greeting";
    }

    struct Counter(usize);
    impl TypedService for Counter {
        const NAME: &'static str = "counter";
    }

    struct FlakyDep;
    impl TypedService for FlakyDep {
        const NAME: &'static str = "flaky-dep";
    }

    struct Out;
    impl TypedService for Out {
        const NAME: &'static str = "out";
    }

    struct HelloPlugin;
    impl Plugin for HelloPlugin {
        fn name(&self) -> &'static str {
            "hello"
        }
        fn provides(&self) -> &[&'static str] {
            &["greeting"]
        }
        fn apply(
            &self,
            ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let ctx = ctx.clone();
            Box::pin(async move {
                ctx.provide::<Greeting>(Arc::new(Greeting(Arc::from("hello"))))
                    .map(|handle| vec![handle])
                    .map_err(anyhow::Error::from)
            })
        }
    }

    struct ForgetfulPlugin;
    impl Plugin for ForgetfulPlugin {
        fn name(&self) -> &'static str {
            "forgetful"
        }
        fn provides(&self) -> &[&'static str] {
            &["greeting"]
        }
        fn apply(
            &self,
            ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let ctx = ctx.clone();
            Box::pin(async move {
                // Deliberately forget to collect the returned handle.
                let _ = ctx.provide::<Greeting>(Arc::new(Greeting(Arc::from("forgot"))));
                Ok(vec![])
            })
        }
    }

    struct CounterPlugin;
    impl Plugin for CounterPlugin {
        fn name(&self) -> &'static str {
            "counter"
        }
        fn provides(&self) -> &[&'static str] {
            &["counter"]
        }
        fn apply(
            &self,
            ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let ctx = ctx.clone();
            Box::pin(async move {
                ctx.provide::<Counter>(Arc::new(Counter(5)))
                    .map(|handle| vec![handle])
                    .map_err(anyhow::Error::from)
            })
        }
    }

    struct NeedsGreeting;
    impl Plugin for NeedsGreeting {
        fn name(&self) -> &'static str {
            "needs-greeting"
        }
        fn inject(&self) -> Inject {
            Inject::new(vec!["greeting".to_owned()])
        }
        fn apply(
            &self,
            _ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    struct NeedsMissing;
    impl Plugin for NeedsMissing {
        fn name(&self) -> &'static str {
            "needs-missing"
        }
        fn inject(&self) -> Inject {
            Inject::new(vec!["missing".to_owned()])
        }
        fn apply(
            &self,
            _ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    struct MissingSvc;
    impl TypedService for MissingSvc {
        const NAME: &'static str = "missing";
    }

    struct PreSvc;
    impl TypedService for PreSvc {
        const NAME: &'static str = "pre-svc";
    }

    struct NeedsPre;
    impl Plugin for NeedsPre {
        fn name(&self) -> &'static str {
            "needs-pre"
        }
        fn inject(&self) -> Inject {
            Inject::new(vec!["pre-svc".to_owned()])
        }
        fn apply(
            &self,
            _ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    struct DynProvider;
    impl Plugin for DynProvider {
        fn name(&self) -> &'static str {
            "dyn-provider"
        }
        fn apply(
            &self,
            ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let ctx = ctx.clone();
            Box::pin(async move {
                ctx.provide_named::<Counter>("dyn:svc".to_owned(), Arc::new(Counter(1)))
                    .map(|handle| vec![handle])
                    .map_err(anyhow::Error::from)
            })
        }
    }

    struct DynConsumer;
    impl Plugin for DynConsumer {
        fn name(&self) -> &'static str {
            "dyn-consumer"
        }
        fn inject(&self) -> Inject {
            Inject::new(vec!["dyn:svc".to_owned()])
        }
        fn apply(
            &self,
            _ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    struct MultiConsumer;
    impl Plugin for MultiConsumer {
        fn name(&self) -> &'static str {
            "multi-consumer"
        }
        fn inject(&self) -> Inject {
            Inject::new(vec!["greeting".to_owned(), "counter".to_owned()])
        }
        fn apply(
            &self,
            _ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    struct ChildSvc;
    impl TypedService for ChildSvc {
        const NAME: &'static str = "child-svc";
    }

    struct Child;
    impl Plugin for Child {
        fn name(&self) -> &'static str {
            "child"
        }
        fn provides(&self) -> &[&'static str] {
            &["child-svc"]
        }
        fn apply(
            &self,
            ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let ctx = ctx.clone();
            Box::pin(async move {
                ctx.provide::<ChildSvc>(Arc::new(ChildSvc))
                    .map(|handle| vec![handle])
                    .map_err(anyhow::Error::from)
            })
        }
    }

    struct Parent;
    impl Plugin for Parent {
        fn name(&self) -> &'static str {
            "parent"
        }
        fn apply(
            &self,
            ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let ctx = ctx.clone();
            Box::pin(async move {
                let _child = ctx.plugin(Child).await?;
                Ok(vec![])
            })
        }
    }

    struct AsyncEffectPlugin {
        ran: Arc<AtomicBool>,
    }
    impl Plugin for AsyncEffectPlugin {
        fn name(&self) -> &'static str {
            "async-effect"
        }
        fn apply(
            &self,
            _ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let ran = Arc::clone(&self.ran);
            Box::pin(async move {
                let effect = EffectHandle::new_async(move || {
                    let ran = Arc::clone(&ran);
                    Box::pin(async move {
                        tokio::task::yield_now().await;
                        ran.store(true, Ordering::SeqCst);
                    })
                });
                Ok(vec![effect])
            })
        }
    }

    struct AsyncChild {
        ran: Arc<AtomicBool>,
    }
    impl Plugin for AsyncChild {
        fn name(&self) -> &'static str {
            "async-child"
        }
        fn apply(
            &self,
            ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let ctx = ctx.clone();
            let ran = Arc::clone(&self.ran);
            Box::pin(async move {
                let service = ctx
                    .provide::<ChildSvc>(Arc::new(ChildSvc))
                    .map_err(anyhow::Error::from)?;
                let async_effect = EffectHandle::new_async(move || {
                    let ran = Arc::clone(&ran);
                    Box::pin(async move {
                        ran.store(true, Ordering::SeqCst);
                    })
                });
                Ok(vec![service, async_effect])
            })
        }
    }

    struct AsyncParent {
        child: Arc<AtomicBool>,
    }
    impl Plugin for AsyncParent {
        fn name(&self) -> &'static str {
            "async-parent"
        }
        fn apply(
            &self,
            ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let ctx = ctx.clone();
            let child = Arc::clone(&self.child);
            Box::pin(async move {
                let _ = ctx.plugin(AsyncChild { ran: child }).await?;
                Ok(vec![])
            })
        }
    }

    struct FailingPlugin;
    impl Plugin for FailingPlugin {
        fn name(&self) -> &'static str {
            "failing"
        }
        fn apply(
            &self,
            _ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            Box::pin(async { anyhow::bail!("plugin failed") })
        }
    }

    struct A;
    impl Plugin for A {
        fn name(&self) -> &'static str {
            "a"
        }
        fn inject(&self) -> Inject {
            Inject::new(vec!["b".to_owned()])
        }
        fn provides(&self) -> &[&'static str] {
            &["a"]
        }
        fn apply(
            &self,
            _ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    struct B;
    impl Plugin for B {
        fn name(&self) -> &'static str {
            "b"
        }
        fn inject(&self) -> Inject {
            Inject::new(vec!["a".to_owned()])
        }
        fn provides(&self) -> &[&'static str] {
            &["b"]
        }
        fn apply(
            &self,
            _ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            Box::pin(async { Ok(vec![]) })
        }
    }

    struct DepProvider;
    impl Plugin for DepProvider {
        fn name(&self) -> &'static str {
            "dep-provider"
        }
        fn provides(&self) -> &[&'static str] {
            &["flaky-dep"]
        }
        fn apply(
            &self,
            ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let ctx = ctx.clone();
            Box::pin(async move {
                ctx.provide::<FlakyDep>(Arc::new(FlakyDep))
                    .map(|handle| vec![handle])
                    .map_err(anyhow::Error::from)
            })
        }
    }

    struct Consumer {
        calls: Arc<AtomicUsize>,
    }
    impl Plugin for Consumer {
        fn name(&self) -> &'static str {
            "consumer"
        }
        fn inject(&self) -> Inject {
            Inject::new(vec!["flaky-dep".to_owned()])
        }
        fn provides(&self) -> &[&'static str] {
            &["out"]
        }
        fn apply(
            &self,
            ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let calls = Arc::clone(&self.calls);
            let ctx = ctx.clone();
            Box::pin(async move {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    ctx.provide::<Out>(Arc::new(Out))
                        .map(|handle| vec![handle])
                        .map_err(anyhow::Error::from)
                } else {
                    anyhow::bail!("reload failed")
                }
            })
        }
    }

    struct StableConsumer;
    impl Plugin for StableConsumer {
        fn name(&self) -> &'static str {
            "stable-consumer"
        }
        fn inject(&self) -> Inject {
            Inject::new(vec!["flaky-dep".to_owned()])
        }
        fn provides(&self) -> &[&'static str] {
            &["out"]
        }
        fn apply(
            &self,
            ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let ctx = ctx.clone();
            Box::pin(async move {
                ctx.provide::<Out>(Arc::new(Out))
                    .map(|handle| vec![handle])
                    .map_err(anyhow::Error::from)
            })
        }
    }

    struct HangingPlugin;
    impl Plugin for HangingPlugin {
        fn name(&self) -> &'static str {
            "hanging"
        }
        fn apply(
            &self,
            _ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                Ok(vec![])
            })
        }
    }

    struct CountingConsumer {
        calls: Arc<AtomicUsize>,
    }
    impl Plugin for CountingConsumer {
        fn name(&self) -> &'static str {
            "counting-consumer"
        }
        fn inject(&self) -> Inject {
            Inject::new(vec!["flaky-dep".to_owned()])
        }
        fn apply(
            &self,
            _ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![])
            })
        }
    }

    #[tokio::test]
    async fn registry_boots_plugin_and_provides_service() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(HelloPlugin);
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert_eq!(ctx.require::<Greeting>().unwrap().0.as_ref(), "hello");
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].name(), "hello");
        assert_eq!(handles[0].state(), FiberState::Active);
        handles[0].shutdown().await;
        assert_eq!(handles[0].state(), FiberState::Disposed);
        assert!(!ctx.has("greeting"));
    }

    #[tokio::test]
    async fn forgotten_service_registration_is_auto_adopted() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(ForgetfulPlugin);
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert_eq!(ctx.require::<Greeting>().unwrap().0.as_ref(), "forgot");
        handles[0].shutdown().await;
        assert!(!ctx.has("greeting"));
    }

    #[tokio::test]
    async fn boot_with_no_plugins_returns_empty() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.is_empty());
    }

    #[tokio::test]
    async fn boot_runs_multiple_plugins() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(HelloPlugin);
        registry.register(CounterPlugin);
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert_eq!(handles.len(), 2);
        assert_eq!(ctx.require::<Greeting>().unwrap().0.as_ref(), "hello");
        assert_eq!(ctx.require::<Counter>().unwrap().0, 5);
    }

    #[tokio::test]
    async fn boot_propagates_plugin_error() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(FailingPlugin);
        assert!(registry.boot(&ctx, &bus).await.is_err());
    }

    #[tokio::test]
    async fn boot_failure_cleans_up_partial_activation() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(HelloPlugin);
        registry.register(FailingPlugin);
        assert!(registry.boot(&ctx, &bus).await.is_err());
        assert!(!ctx.has("greeting"));
    }

    #[tokio::test]
    async fn boot_respects_inject_dependencies() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(HelloPlugin);
        registry.register(NeedsGreeting);
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert_eq!(handles.len(), 2);
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
    }

    #[tokio::test]
    async fn boot_is_order_independent() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(NeedsGreeting);
        registry.register(HelloPlugin);
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert_eq!(handles.len(), 2);
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
    }

    #[tokio::test]
    async fn boot_leaves_unresolved_dependency_pending() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(NeedsMissing);
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].state(), FiberState::Pending);
    }

    #[tokio::test]
    async fn runtime_provider_activates_pending_plugin() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(NeedsMissing);
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert_eq!(handles[0].state(), FiberState::Pending);

        let _h = ctx.provide::<MissingSvc>(Arc::new(MissingSvc));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert_eq!(handles[0].state(), FiberState::Active);
    }

    #[tokio::test]
    async fn root_provided_service_satisfies_inject() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let _pre = ctx.provide::<PreSvc>(Arc::new(PreSvc));
        let registry = PluginRegistry::new();
        registry.register(NeedsPre);
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert_eq!(handles[0].state(), FiberState::Active);
    }

    #[tokio::test]
    async fn boot_detects_dependency_cycle() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(A);
        registry.register(B);
        assert!(registry.boot(&ctx, &bus).await.is_err());
    }

    #[tokio::test]
    async fn reload_failure_clears_old_effects_and_fails() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(DepProvider);
        registry.register(Consumer {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
        assert!(ctx.has("out"));

        let result = handles[1].update(Config::new(())).await;
        assert!(result.is_err());
        assert!(!ctx.has("out"));
        assert_eq!(handles[1].state(), FiberState::Failed);
    }

    #[tokio::test]
    async fn removing_dependency_unloads_consumer() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(DepProvider);
        registry.register(Consumer {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(ctx.has("out"));

        handles[0].shutdown().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert!(!ctx.has("out"));
    }

    #[tokio::test]
    async fn replacing_dependency_reloads_consumer_successfully() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(DepProvider);
        registry.register(StableConsumer);
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(ctx.has("out"));

        handles[0].shutdown().await;
        let _replacement = ctx.provide::<FlakyDep>(Arc::new(FlakyDep)).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert!(ctx.has("out"));
    }

    #[tokio::test]
    async fn boot_apply_timeout_is_enforced() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.set_apply_timeout(std::time::Duration::from_millis(10));
        registry.register(HangingPlugin);
        match registry.boot(&ctx, &bus).await {
            Err(err) => assert!(err.to_string().contains("timed out")),
            Ok(_) => panic!("expected apply timeout error"),
        }
    }

    #[tokio::test]
    async fn replacing_dependency_only_reconciles_dependents() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(DepProvider);
        let calls = Arc::new(AtomicUsize::new(0));
        registry.register(CountingConsumer {
            calls: Arc::clone(&calls),
        });
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        let boot_calls = calls.load(Ordering::SeqCst);

        handles[0].shutdown().await;
        let _replacement = ctx.provide::<FlakyDep>(Arc::new(FlakyDep)).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert!(calls.load(Ordering::SeqCst) > boot_calls);
    }

    #[tokio::test]
    async fn dynamic_inject_resolves_via_runtime_provider() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(DynProvider);
        registry.register(DynConsumer);
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
        assert_eq!(ctx.require_named::<Counter>("dyn:svc").unwrap().0, 1);
    }

    #[tokio::test]
    async fn boot_satisfies_multiple_inject_dependencies() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(HelloPlugin);
        registry.register(CounterPlugin);
        registry.register(MultiConsumer);
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
    }

    #[tokio::test]
    async fn shutdown_pending_plugin_disposes_it() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(NeedsMissing);
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert_eq!(handles[0].state(), FiberState::Pending);
        handles[0].shutdown().await;
        assert_eq!(handles[0].state(), FiberState::Disposed);
    }

    #[tokio::test]
    async fn ctx_plugin_mounts_child_and_disposes_recursively() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(Parent);
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert_eq!(handles.len(), 1);
        assert!(ctx.has("child-svc"));

        handles[0].shutdown().await;
        assert!(!ctx.has("child-svc"));
    }

    #[tokio::test]
    async fn async_disposer_runs_on_shutdown() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        let ran = Arc::new(AtomicBool::new(false));
        registry.register(AsyncEffectPlugin {
            ran: Arc::clone(&ran),
        });
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(!ran.load(Ordering::SeqCst));
        handles[0].shutdown().await;
        assert!(ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn child_async_disposer_runs_when_parent_disposed() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        let ran = Arc::new(AtomicBool::new(false));
        registry.register(AsyncParent {
            child: Arc::clone(&ran),
        });
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(ctx.has("child-svc"));
        assert!(!ran.load(Ordering::SeqCst));

        handles[0].shutdown().await;
        assert!(ran.load(Ordering::SeqCst));
        assert!(!ctx.has("child-svc"));
    }

    #[tokio::test]
    async fn plugin_sync_form_boots() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(plugin_sync("sync", Inject::none(), |ctx, _bus| {
            ctx.provide::<Greeting>(Arc::new(Greeting(Arc::from("sync"))))
                .map(|handle| vec![handle])
                .map_err(anyhow::Error::from)
        }));
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
        assert_eq!(ctx.require::<Greeting>().unwrap().0.as_ref(), "sync");
    }

    #[tokio::test]
    async fn plugin_async_form_boots() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(plugin_async("async", Inject::none(), |ctx, _bus| {
            let ctx = ctx.clone();
            Box::pin(async move {
                ctx.provide::<Greeting>(Arc::new(Greeting(Arc::from("async"))))
                    .map(|handle| vec![handle])
                    .map_err(anyhow::Error::from)
            })
        }));
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
        assert_eq!(ctx.require::<Greeting>().unwrap().0.as_ref(), "async");
    }

    #[tokio::test]
    async fn service_sync_form_boots() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(service_sync::<Greeting, _>(
            "service-sync",
            Inject::none(),
            |_ctx| Ok(Greeting(Arc::from("svc-sync"))),
        ));
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
        assert_eq!(ctx.require::<Greeting>().unwrap().0.as_ref(), "svc-sync");
    }

    #[tokio::test]
    async fn service_async_form_boots() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(service_async::<Greeting, _>(
            "service-async",
            Inject::none(),
            |_ctx| Box::pin(async { Ok(Greeting(Arc::from("svc-async"))) }),
        ));
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
        assert_eq!(ctx.require::<Greeting>().unwrap().0.as_ref(), "svc-async");
    }

    #[derive(Clone)]
    struct GreetingConfig {
        text: Arc<str>,
    }

    #[tokio::test]
    async fn plugin_sync_with_config_receives_typed_config() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register_with_config(
            plugin_sync_with_config::<GreetingConfig, _>(
                "configured-sync",
                Inject::none(),
                |ctx, _bus, config| {
                    ctx.provide::<Greeting>(Arc::new(Greeting(config.text.clone())))
                        .map(|handle| vec![handle])
                        .map_err(anyhow::Error::from)
                },
            ),
            Config::new(GreetingConfig {
                text: Arc::from("configured"),
            }),
        );
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
        assert_eq!(ctx.require::<Greeting>().unwrap().0.as_ref(), "configured");
    }

    #[tokio::test]
    async fn plugin_async_with_config_receives_typed_config() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register_with_config(
            plugin_async_with_config::<GreetingConfig, _>(
                "configured-async",
                Inject::none(),
                |ctx, _bus, config| {
                    let ctx = ctx.clone();
                    let text = config.text.clone();
                    Box::pin(async move {
                        ctx.provide::<Greeting>(Arc::new(Greeting(text)))
                            .map(|handle| vec![handle])
                            .map_err(anyhow::Error::from)
                    })
                },
            ),
            Config::new(GreetingConfig {
                text: Arc::from("async-configured"),
            }),
        );
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
        assert_eq!(
            ctx.require::<Greeting>().unwrap().0.as_ref(),
            "async-configured"
        );
    }

    #[tokio::test]
    async fn service_sync_with_config_constructs_service() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register_with_config(
            service_sync_with_config::<Greeting, GreetingConfig, _>(
                "configured-service-sync",
                Inject::none(),
                |_ctx, config| Ok(Greeting(config.text.clone())),
            ),
            Config::new(GreetingConfig {
                text: Arc::from("svc-configured"),
            }),
        );
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
        assert_eq!(
            ctx.require::<Greeting>().unwrap().0.as_ref(),
            "svc-configured"
        );
    }

    #[tokio::test]
    async fn service_async_with_config_constructs_service() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register_with_config(
            service_async_with_config::<Greeting, GreetingConfig, _>(
                "configured-service-async",
                Inject::none(),
                |_ctx, config| {
                    let text = config.text.clone();
                    Box::pin(async move { Ok(Greeting(text)) })
                },
            ),
            Config::new(GreetingConfig {
                text: Arc::from("svc-async-configured"),
            }),
        );
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
        assert_eq!(
            ctx.require::<Greeting>().unwrap().0.as_ref(),
            "svc-async-configured"
        );
    }

    #[tokio::test]
    async fn wrong_config_type_fails_activation() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register_with_config(
            plugin_sync_with_config::<GreetingConfig, _>(
                "configured-sync",
                Inject::none(),
                |_, _, _| Ok(vec![]),
            ),
            Config::new(42u64),
        );
        assert!(registry.boot(&ctx, &bus).await.is_err());
    }

    struct ValidatedPlugin {
        seen: Arc<Mutex<Vec<u64>>>,
    }

    impl Plugin for ValidatedPlugin {
        fn name(&self) -> &'static str {
            "validated"
        }

        fn validate_config(&self, config: Config) -> anyhow::Result<Config> {
            let value = config
                .downcast::<u64>()
                .map_err(|error| anyhow::anyhow!(error))?;
            if *value == 0 {
                anyhow::bail!("config must be positive");
            }
            Ok(config)
        }

        fn apply_with_config(
            &self,
            ctx: &Context,
            _bus: &EventBus,
            config: Config,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let value = config.downcast::<u64>().unwrap();
            self.seen.lock().expect("seen poisoned").push(*value);
            let provided = ctx.provide_any("validated-count", Value::new(*value));
            Box::pin(async move {
                let effect = provided.map_err(anyhow::Error::from)?;
                Ok(vec![effect])
            })
        }
    }

    #[tokio::test]
    async fn custom_validate_config_rejects_invalid_and_accepts_valid() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        let seen = Arc::new(Mutex::new(Vec::<u64>::new()));
        registry.register_with_config(
            ValidatedPlugin {
                seen: Arc::clone(&seen),
            },
            Config::new(0u64),
        );
        assert!(registry.boot(&ctx, &bus).await.is_err());

        let registry = PluginRegistry::new();
        registry.register_with_config(
            ValidatedPlugin {
                seen: Arc::clone(&seen),
            },
            Config::new(3u64),
        );
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
        assert_eq!(
            *ctx.require_any("validated-count")
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            3
        );
        assert_eq!(*seen.lock().unwrap(), vec![3]);
    }

    #[tokio::test]
    async fn internal_config_waterfall_transforms_config() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        let _intercept = bus
            .dynamic()
            .on_waterfall("internal/config", |_event, next| {
                Box::pin(async move {
                    let _ = next().await?;
                    Ok(Some(Value::new(99u64)))
                })
            });
        registry.register_with_config(
            plugin_sync_with_config::<u64, _>(
                "counter-config",
                Inject::none(),
                |ctx, _bus, config| {
                    ctx.provide_any("configured-count", Value::new(*config))
                        .map(|handle| vec![handle])
                        .map_err(anyhow::Error::from)
                },
            ),
            Config::new(1u64),
        );
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
        assert_eq!(
            *ctx.require_any("configured-count")
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            99
        );
    }

    #[tokio::test]
    async fn plugin_update_hot_reloads_with_new_config() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        let seen = Arc::new(Mutex::new(Vec::<u64>::new()));
        let seen_for_closure = Arc::clone(&seen);
        registry.register_with_config(
            plugin_sync_with_config::<u64, _>(
                "updatable",
                Inject::none(),
                move |ctx, _bus, config| {
                    seen_for_closure.lock().unwrap().push(*config);
                    ctx.provide_any("updatable-count", Value::new(*config))
                        .map(|handle| vec![handle])
                        .map_err(anyhow::Error::from)
                },
            ),
            Config::new(1u64),
        );
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert_eq!(
            *ctx.require_any("updatable-count")
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            1
        );

        handles[0].update(Config::new(2u64)).await.unwrap();
        assert_eq!(
            *ctx.require_any("updatable-count")
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            2
        );
        assert_eq!(*seen.lock().unwrap(), vec![1, 2]);
    }

    #[tokio::test]
    async fn internal_update_waterfall_transforms_updated_config() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register_with_config(
            plugin_sync_with_config::<u64, _>(
                "updatable-transform",
                Inject::none(),
                |ctx, _bus, config| {
                    ctx.provide_any("updatable-count", Value::new(*config))
                        .map(|handle| vec![handle])
                        .map_err(anyhow::Error::from)
                },
            ),
            Config::new(1u64),
        );
        let handles = registry.boot(&ctx, &bus).await.unwrap();

        let _intercept = bus
            .dynamic()
            .on_waterfall("internal/update", |_event, _next| {
                Box::pin(async { Ok(Some(Value::new(7u64))) })
            });
        handles[0].update(Config::new(2u64)).await.unwrap();
        assert_eq!(
            *ctx.require_any("updatable-count")
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            7
        );
    }

    #[test]
    fn inject_builder_helpers_and_conversions() {
        let empty = Inject::none();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert!(empty.names().next().is_none());

        let from_array = Inject::from(["a", "b"]);
        assert_eq!(from_array.names().collect::<Vec<_>>(), vec!["a", "b"]);
        assert!(from_array.contains("a"));
        assert!(!from_array.contains("c"));

        let from_vec = Inject::from(vec!["x".to_owned(), "y".to_owned()]);
        assert_eq!(from_vec.len(), 2);
        assert_eq!(from_vec.names().collect::<Vec<_>>(), vec!["x", "y"]);

        let built = Inject::none()
            .require("svc")
            .require_with("with-config", 7u64)
            .require_with_value("with-value", Config::new(9u64));
        assert_eq!(built.len(), 3);
        assert!(built.contains("svc"));
        assert_eq!(
            built.iter().map(|d| d.config.is_some()).collect::<Vec<_>>(),
            vec![false, true, true]
        );
    }

    #[tokio::test]
    async fn inject_require_with_applies_intercept_config() {
        let seen = Arc::new(Mutex::new(None::<u64>));

        struct GreetingProvider;
        impl Plugin for GreetingProvider {
            fn name(&self) -> &'static str {
                "greeting-provider"
            }
            fn provides(&self) -> &[&'static str] {
                &["greeting"]
            }
            fn apply(
                &self,
                ctx: &Context,
                _bus: &EventBus,
            ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
                let ctx = ctx.clone();
                Box::pin(async move {
                    ctx.provide::<Greeting>(Arc::new(Greeting(Arc::from("hi"))))
                        .map(|handle| vec![handle])
                        .map_err(anyhow::Error::from)
                })
            }
        }

        struct InterceptConsumer {
            seen: Arc<Mutex<Option<u64>>>,
        }
        impl Plugin for InterceptConsumer {
            fn name(&self) -> &'static str {
                "intercept-consumer"
            }
            fn inject(&self) -> Inject {
                Inject::none().require_with("greeting", 42u64)
            }
            fn apply(
                &self,
                ctx: &Context,
                _bus: &EventBus,
            ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
                let seen = self.seen.clone();
                let intercepted = ctx
                    .intercept_value("greeting")
                    .and_then(|value| value.downcast::<u64>().ok().map(|value| *value));
                Box::pin(async move {
                    *seen.lock().expect("seen poisoned") = intercepted;
                    Ok(vec![])
                })
            }
        }

        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(GreetingProvider);
        registry.register(InterceptConsumer {
            seen: Arc::clone(&seen),
        });
        registry.boot(&ctx, &bus).await.unwrap();

        assert_eq!(*seen.lock().expect("seen poisoned"), Some(42u64));
    }

    #[test]
    fn plugin_key_roundtrip() {
        let key = PluginKey::from_raw(42);
        assert_eq!(key.as_raw(), 42);
    }

    struct ConfigOnlyPlugin;
    impl Plugin for ConfigOnlyPlugin {
        fn name(&self) -> &'static str {
            "config-only"
        }
    }

    #[tokio::test]
    async fn plugin_without_apply_returns_requires_config_error() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(ConfigOnlyPlugin);

        let error = match registry.boot(&ctx, &bus).await {
            Ok(_) => panic!("expected boot to fail"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("requires config"));
    }

    #[tokio::test]
    async fn internal_config_error_propagates_from_boot() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let _intercept = bus
            .dynamic()
            .on_waterfall("internal/config", |_event, _next| {
                Box::pin(async { Err(EventError::Listener("config boom".into())) })
            });
        let registry = PluginRegistry::new();
        registry.register_with_config(
            plugin_sync_with_config::<u64, _>("config-error", Inject::none(), |_, _, _| Ok(vec![])),
            Config::new(1u64),
        );

        let error = match registry.boot(&ctx, &bus).await {
            Ok(_) => panic!("expected boot to fail"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("config boom"));
    }

    #[tokio::test]
    async fn internal_config_none_falls_back_to_raw_config() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let _intercept = bus
            .dynamic()
            .on_waterfall("internal/config", |_event, _next| {
                Box::pin(async { Ok(None) })
            });
        let registry = PluginRegistry::new();
        registry.register_with_config(
            plugin_sync_with_config::<u64, _>(
                "config-fallback",
                Inject::none(),
                |ctx, _bus, config| {
                    ctx.provide_any("config-fallback-value", Value::new(*config))
                        .map(|handle| vec![handle])
                        .map_err(anyhow::Error::from)
                },
            ),
            Config::new(9u64),
        );

        registry.boot(&ctx, &bus).await.unwrap();
        assert_eq!(
            *ctx.require_any("config-fallback-value")
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            9
        );
    }

    #[tokio::test]
    async fn plugin_update_after_remove_errors() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register_with_config(
            plugin_sync_with_config::<u64, _>("removable", Inject::none(), |ctx, _bus, config| {
                ctx.provide_any("removable-count", Value::new(*config))
                    .map(|handle| vec![handle])
                    .map_err(anyhow::Error::from)
            }),
            Config::new(1u64),
        );
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert_eq!(
            *ctx.require_any("removable-count")
                .unwrap()
                .downcast::<u64>()
                .unwrap(),
            1
        );

        assert!(ctx.remove_plugin(handles[0].key()).await);
        let error = match handles[0].update(Config::new(2u64)).await {
            Ok(_) => panic!("expected update after removal to fail"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("has been removed"));
    }

    /// A chain link injects `from` and provides `to`, counting applies so
    /// tests can assert exactly how far a reconcile wave got.
    struct ChainLink {
        name: &'static str,
        from: String,
        to: String,
        applies: Arc<AtomicUsize>,
    }

    impl Plugin for ChainLink {
        fn name(&self) -> &'static str {
            self.name
        }
        fn inject(&self) -> Inject {
            Inject::new(vec![self.from.clone()])
        }
        fn apply(
            &self,
            ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let ctx = ctx.clone();
            let to = self.to.clone();
            let applies = Arc::clone(&self.applies);
            Box::pin(async move {
                applies.fetch_add(1, Ordering::SeqCst);
                ctx.provide_any(to, Value::new(1u64))
                    .map(|handle| vec![handle])
                    .map_err(anyhow::Error::from)
            })
        }
    }

    fn chain(len: usize, applies: &Arc<AtomicUsize>) -> Vec<ChainLink> {
        (0..len)
            .map(|i| ChainLink {
                name: Box::leak(format!("chain-{i}").into_boxed_str()),
                from: format!("svc-{i}"),
                to: format!("svc-{}", i + 1),
                applies: Arc::clone(applies),
            })
            .collect()
    }

    #[tokio::test]
    async fn reconcile_wave_cap_brakes_and_recovers() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        // Wave budget of 3: the 4th queued reconcile trips the brake before
        // it executes, so a 5-link cascade cannot finish in one wave.
        registry.set_reconcile_iteration_caps(3, 16);
        let applies = Arc::new(AtomicUsize::new(0));
        for link in chain(5, &applies) {
            registry.register(link);
        }
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Pending));

        let _seed = ctx.provide_any("svc-0", Value::new(0u64)).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // The wave braked after three applies instead of cascading to the
        // end (and instead of spinning forever on a non-converging graph).
        assert_eq!(applies.load(Ordering::SeqCst), 3);
        assert!(ctx.has("svc-3"));
        assert!(!ctx.has("svc-4"));
        assert_eq!(handles[3].state(), FiberState::Pending);

        // Recovery: dispose the third link (freeing the svc-3 slot), then
        // re-provide svc-3 from the root. The new notification starts a
        // fresh wave with a full budget and the chain completes.
        handles[2].shutdown().await;
        let _reseed = ctx.provide_any("svc-3", Value::new(0u64)).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert_eq!(applies.load(Ordering::SeqCst), 5);
        assert!(ctx.has("svc-5"));
        assert_eq!(handles[3].state(), FiberState::Active);
        assert_eq!(handles[4].state(), FiberState::Active);
    }

    #[tokio::test]
    async fn reconcile_per_plugin_cap_brakes_without_hang() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        // A per-plugin budget of 0 trips the brake before the plugin's first
        // reconcile executes; the worker must still terminate promptly.
        registry.set_reconcile_iteration_caps(1000, 0);
        let applies = Arc::new(AtomicUsize::new(0));
        for link in chain(2, &applies) {
            registry.register(link);
        }
        let handles = registry.boot(&ctx, &bus).await.unwrap();

        let _seed = ctx.provide_any("svc-0", Value::new(0u64)).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert_eq!(applies.load(Ordering::SeqCst), 0);
        assert!(!ctx.has("svc-1"));
        assert_eq!(handles[0].state(), FiberState::Pending);
        assert_eq!(handles[1].state(), FiberState::Pending);
    }

    #[tokio::test]
    async fn reconcile_default_budget_covers_normal_cascade() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        let applies = Arc::new(AtomicUsize::new(0));
        for link in chain(8, &applies) {
            registry.register(link);
        }
        let handles = registry.boot(&ctx, &bus).await.unwrap();

        let _seed = ctx.provide_any("svc-0", Value::new(0u64)).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // A legit 8-link cascade (one reconcile per plugin) stays far below
        // the default wave budget and converges without braking.
        assert_eq!(applies.load(Ordering::SeqCst), 8);
        assert!(ctx.has("svc-8"));
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
    }

    // ---- WI-45: caller-fiber effect ownership locking tests ----

    struct ProbeEvent;
    impl crate::events::Event for ProbeEvent {
        const NAME: &'static str = "probe/event";
    }

    /// Registers a listener through the scoped apply context without
    /// returning the handle; caller-fiber ownership must still tie the
    /// listener to the plugin's fiber.
    struct ListenerPlugin {
        hits: Arc<AtomicUsize>,
    }
    impl Plugin for ListenerPlugin {
        fn name(&self) -> &'static str {
            "listener"
        }
        fn apply(
            &self,
            ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let ctx = ctx.clone();
            let hits = Arc::clone(&self.hits);
            Box::pin(async move {
                let _ = ctx.on::<ProbeEvent, _>(move |_event| {
                    let hits = Arc::clone(&hits);
                    Box::pin(async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Ok(Value::new(()))
                    })
                });
                Ok(vec![])
            })
        }
    }

    /// No injected dependencies and no provides; its returned effect must be
    /// untouched by any other plugin's reload.
    struct UnrelatedPlugin {
        disposes: Arc<AtomicUsize>,
        applies: Arc<AtomicUsize>,
    }
    impl Plugin for UnrelatedPlugin {
        fn name(&self) -> &'static str {
            "unrelated"
        }
        fn apply(
            &self,
            _ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let disposes = Arc::clone(&self.disposes);
            let applies = Arc::clone(&self.applies);
            Box::pin(async move {
                applies.fetch_add(1, Ordering::SeqCst);
                Ok(vec![EffectHandle::new(move || {
                    disposes.fetch_add(1, Ordering::SeqCst);
                })])
            })
        }
    }

    /// Injects `flaky-dep` and returns a single effect so tests can observe
    /// when its own fiber is cleared.
    struct EffectfulConsumer {
        disposes: Arc<AtomicUsize>,
    }
    impl Plugin for EffectfulConsumer {
        fn name(&self) -> &'static str {
            "effectful-consumer"
        }
        fn inject(&self) -> Inject {
            Inject::new(vec!["flaky-dep".to_owned()])
        }
        fn apply(
            &self,
            _ctx: &Context,
            _bus: &EventBus,
        ) -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
            let disposes = Arc::clone(&self.disposes);
            Box::pin(async move {
                Ok(vec![EffectHandle::new(move || {
                    disposes.fetch_add(1, Ordering::SeqCst);
                })])
            })
        }
    }

    #[tokio::test]
    async fn plugin_reload_clears_listeners_registered_in_apply() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        let hits = Arc::new(AtomicUsize::new(0));
        registry.register(ListenerPlugin {
            hits: Arc::clone(&hits),
        });
        let handles = registry.boot(&ctx, &bus).await.unwrap();

        bus.emit(Arc::new(ProbeEvent));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Hot reload: the old incarnation's listener is disposed with the
        // plugin's own fiber and a fresh one is registered.
        handles[0].update(Config::new(())).await.unwrap();
        bus.emit(Arc::new(ProbeEvent));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        // Exactly one more hit: a leaked pre-reload listener would double it.
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        handles[0].shutdown().await;
        bus.emit(Arc::new(ProbeEvent));
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn provider_reload_leaves_unrelated_plugins_untouched() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(DepProvider);
        let disposes = Arc::new(AtomicUsize::new(0));
        let applies = Arc::new(AtomicUsize::new(0));
        registry.register(UnrelatedPlugin {
            disposes: Arc::clone(&disposes),
            applies: Arc::clone(&applies),
        });
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert_eq!(applies.load(Ordering::SeqCst), 1);

        // Reloading the provider only clears the provider's own fiber.
        handles[0].update(Config::new(())).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert_eq!(applies.load(Ordering::SeqCst), 1);
        assert_eq!(disposes.load(Ordering::SeqCst), 0);
        assert_eq!(handles[1].state(), FiberState::Active);
        assert!(ctx.has("flaky-dep"));
    }

    #[tokio::test]
    async fn dependent_effects_cleared_by_own_reconcile_on_provider_removal() {
        let ctx = Context::new();
        let bus = EventBus::new();
        let registry = PluginRegistry::new();
        registry.register(DepProvider);
        let disposes = Arc::new(AtomicUsize::new(0));
        registry.register(EffectfulConsumer {
            disposes: Arc::clone(&disposes),
        });
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        assert!(handles.iter().all(|h| h.state() == FiberState::Active));
        assert_eq!(disposes.load(Ordering::SeqCst), 0);

        // Removing the provider does not reach into the dependent's fiber:
        // the dependent's own reconcile unloads it, running its effects
        // exactly once and moving it back to Pending.
        handles[0].shutdown().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert_eq!(disposes.load(Ordering::SeqCst), 1);
        assert_eq!(handles[1].state(), FiberState::Pending);
        assert!(!ctx.has("flaky-dep"));
    }
}
