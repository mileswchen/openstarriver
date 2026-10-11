# Context

The root and child dependency containers used by plugins.

## Types

```rust
pub struct Context;

pub struct Isolation;

pub enum ContextError {
    Missing(String),
    Duplicate(String),
    TypeMismatch { name: String, expected: &'static str, actual: &'static str },
    Event(EventError),
    Cast(ValueCastError),
}

impl Isolation {
    pub const fn from_raw(value: u64) -> Self;
    pub const fn as_raw(self) -> u64;
}
```

## Construction and scoping

```rust
impl Context {
    pub fn new() -> Self;
    pub fn scoped(&self, provider: Weak<Fiber>) -> Self;
    pub fn new_isolation(&self) -> Isolation;
    pub fn isolate(&self, name: impl Into<String>) -> Self;
    pub fn isolate_with(&self, name: impl Into<String>, label: Isolation) -> Self;
    pub fn isolate_many(
        &self,
        names: impl IntoIterator<Item = impl Into<String>>,
        label: Option<Isolation>,
    ) -> Self;
    pub fn with_spawner(&self, spawner: Spawner) -> Self;
    pub fn with_event_bus(&self, bus: &EventBus) -> Self;
    pub fn with_filter<F>(&self, filter: F) -> Self
    where F: Fn(&Context) -> bool + Send + Sync + 'static;
    pub fn filter(&self) -> Option<&ContextFilter>;
    pub fn intercept(&self, name: impl Into<String>, config: Value) -> Self;
    pub fn intercept_typed<T>(&self, name: impl Into<String>, config: T) -> Self;
    pub fn intercepts(&self, name: &str) -> Vec<Value>;
    pub fn intercept_value(&self, name: &str) -> Option<Value>;
}
```

`isolate(name)` gives one service name a fresh, independent implementation
slot; the slot starts empty rather than inheriting the parent's value.
`isolate_with(name, label)` reuses an explicit label, so separate context
branches can join the same slot. `isolate_many(names, label?)` maps several
service names onto one shared slot. `new_isolation` allocates a unique label
without touching a context. `with_event_bus` attaches an event bus so the
async service accessors can dispatch `internal/get` and `internal/set`.

`intercept` returns a child context carrying one additional intercept config for `name`; the parent is not mutated. Intercepts accumulate root-most first and are readable with `intercepts(name)`.

`with_filter` attaches an event-listener filter used by `*_scoped` dispatch: a
listener whose owning context does not pass the filter is skipped. `filter`
returns the attached predicate, if any.

## Events

`Context` is the primary event API. Listener registration records the context
as the owner, so target-filtered dispatch can later decide whether that
listener should run:

```rust
pub fn on<E, F>(&self, listener: F) -> EffectHandle;
pub fn on_sync<E, F>(&self, listener: F) -> EffectHandle;
pub fn once<E, F>(&self, listener: F) -> EffectHandle;
pub fn once_sync<E, F>(&self, listener: F) -> EffectHandle;
pub fn on_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle;
pub fn on_sync_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle;
pub fn on_waterfall<E, F>(&self, listener: F) -> EffectHandle;

pub fn emit<E: Event>(&self, event: Arc<E>);
pub fn emit_sync<E: Event>(&self, event: Arc<E>) -> Vec<Result<Value, EventError>>;
pub async fn serial<E: Event>(&self, event: Arc<E>) -> Result<Option<Value>, EventError>;
pub async fn parallel<E: Event>(&self, event: Arc<E>) -> Vec<Result<Value, EventError>>;
pub async fn bail<E: Event>(&self, event: Arc<E>) -> Result<Option<Value>, EventError>;
pub fn bail_sync<E: Event>(&self, event: Arc<E>) -> Result<Option<Value>, EventError>;
pub async fn waterfall<E, F>(&self, event: Arc<E>, inner: F) -> Result<E::Output, EventError>;
```

These require an attached bus; call `with_event_bus` (or boot a
`PluginRegistry`) first. Dispatch without an explicit target runs every
listener. Use `EventBus::*_scoped` to filter by a target context.

## Effects and ownership

`Context::effect` registers a cleanup on the context branch, disposed by
`Context::dispose_effects`. Listener registrations (`on*`, `once*`,
`on_waterfall`) go through the same tracking.

Ownership rule (caller-fiber semantics): when the context is scoped to a
provider fiber — as it is inside a plugin's `apply` — every effect registered
through that context (listeners, cleanups, provided services) is adopted by
the caller fiber. Reloading or disposing the registrant cleans up exactly the
effects it registered. A provider being deactivated never clears a dependent's
effects; dependents re-evaluate themselves through reconcile (their own fiber
is cleared on re-apply or unload). Contexts without a current fiber (root,
isolated, tagged scopes) keep branch-scope disposal via `dispose_effects`.

## Plugins

```rust
pub fn plugin(&self, child: impl Plugin) -> BoxFuture<anyhow::Result<PluginHandle>>;
pub fn plugin_with_config(&self, child: impl Plugin, config: Config) -> BoxFuture<anyhow::Result<PluginHandle>>;
pub fn remove_plugin(&self, key: PluginKey) -> BoxFuture<bool>;
pub fn replace_plugin(&self, old: PluginKey, new: impl Plugin) -> BoxFuture<anyhow::Result<PluginHandle>>;
pub fn replace_plugin_with_config(&self, old: PluginKey, new: impl Plugin, config: Config) -> BoxFuture<anyhow::Result<PluginHandle>>;
```

Starts a plugin against the runtime registry attached to this context. When a
current fiber exists, the plugin mounts as its child and is disposed together
with that fiber; otherwise it becomes a top-level runtime entry. `plugin`
defaults the config to `()`.

`remove_plugin(key)` disposes a specific running instance by its stable key
and returns `false` when no live instance has that key. `replace_plugin` /
`replace_plugin_with_config` remove the instance identified by `old` and start
the new plugin in the same ownership position.

These methods require a registry attached to the context root. `PluginRegistry::boot`
attaches the registry; contexts constructed manually without a booted registry
fall back to the legacy `Spawner`-only child-mount path, which still requires a
current fiber.

## Services

```rust
pub fn provide<T: TypedService>(&self, value: Arc<T>) -> Result<EffectHandle, ContextError>;
pub fn provide_named<T: Service>(&self, name: String, value: Arc<T>) -> Result<EffectHandle, ContextError>;
pub fn provide_any(&self, name: impl Into<String>, value: Value) -> Result<EffectHandle, ContextError>;
pub fn provide_dyn(&self, name: impl Into<String>, value: Arc<dyn Service>) -> Result<EffectHandle, ContextError>;
pub fn require<T: TypedService>(&self) -> Result<Arc<T>, ContextError>;
pub fn require_named<T: Service>(&self, name: &str) -> Result<Arc<T>, ContextError>;
pub fn require_any(&self, name: &str) -> Option<Value>;
pub fn require_dyn(&self, name: &str) -> Result<Arc<dyn Service>, ContextError>;
pub fn require_async<T: TypedService>(&self) -> BoxFuture<Result<Arc<T>, ContextError>>;
pub fn require_named_async<T: Service>(&self, name: &str) -> BoxFuture<Result<Arc<T>, ContextError>>;
pub fn require_any_async(&self, name: String) -> BoxFuture<Result<Option<Value>, ContextError>>;
pub fn provide_async<T: TypedService>(&self, value: Arc<T>) -> BoxFuture<Result<EffectHandle, ContextError>>;
pub fn provide_named_async<T: Service>(&self, name: String, value: Arc<T>) -> BoxFuture<Result<EffectHandle, ContextError>>;
pub fn provide_any_async(&self, name: String, value: Value) -> BoxFuture<Result<EffectHandle, ContextError>>;
pub fn has(&self, name: &str) -> bool;
pub fn dependency_epoch(&self, inject: &[&str]) -> Option<Vec<u64>>;
pub fn provided_names(&self, fiber: &Arc<Fiber>) -> HashSet<String>;
```

`provide` registration is automatically attached to the current fiber when one is available; otherwise the returned `EffectHandle` must be disposed manually. Providing the same name again in the same scope returns `Err(ContextError::Duplicate)`; dispose the previous handle (or remove the owning plugin) before re-providing.

`provide_any` / `require_any` are the untyped, dynamic-name escape hatch for external plugins that only know their service names and payloads at runtime. They carry a `Value` directly and do not require the concrete type to implement `Service`; callers downcast the result themselves.

`provide_dyn` / `require_dyn` are the object-safe channel for `Arc<dyn Service>` values under an explicit runtime name. They keep the `is_available` availability gate; zero-argument `require::<T>()` cannot target dyn services (there is no instance or type-level name), so dyn access is always name-explicit.

The `*_async` accessors route through the dynamic waterfall when a bus is attached:

- `require_named_async` dispatches `internal/get`; a listener can short-circuit by returning `Some(value)` without calling `next()`.
- `provide_named_async` dispatches `internal/set`; a listener can veto the write by returning `Some(false)` without calling `next()`.

Without an attached bus, the async accessors fall back to the synchronous implementations.

## Change notifications

```rust
pub fn subscribe(&self, callback: Subscriber) -> EffectHandle;
```

## Public seam keys

```rust
pub const KERNEL_PREFIX: &str = "kernel:";
pub const SEAM_PREFIX: &str = "seam:";
pub fn is_public_seam(name: &str) -> bool;
```
