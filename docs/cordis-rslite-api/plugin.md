# Plugin and Registry

Plugin entrypoints and dependency-aware boot.

## Plugin trait

```rust
pub trait Plugin: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn inject(&self) -> Inject { Inject::none() }
    fn provides(&self) -> &[&'static str] { &[] }
    fn validate_config(&self, config: Config) -> anyhow::Result<Config> { Ok(config) }
    fn apply(&self, ctx: &Context, bus: &EventBus)
        -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>>;
    fn apply_with_config(&self, ctx: &Context, bus: &EventBus, config: Config)
        -> BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>>;
}
```

Config-aware plugins override `validate_config` to type-check/validate and
`apply_with_config` to consume the validated config. Legacy plugins keep
implementing only `apply(ctx, bus)`; the registry always routes through
`apply_with_config`, whose default delegates to `apply`.

## Built-in plugin forms

```rust
pub fn plugin_sync<F>(name: &'static str, inject: Inject, callback: F) -> SyncFnPlugin<F>;
pub fn plugin_async<F>(name: &'static str, inject: Inject, callback: F) -> AsyncFnPlugin<F>;
pub fn service_sync<S, F>(name: &'static str, inject: Inject, constructor: F) -> SyncServicePlugin<S, F>;
pub fn service_async<S, F>(name: &'static str, inject: Inject, constructor: F) -> AsyncServicePlugin<S, F>;
pub fn plugin_sync_with_config<C, F>(name, inject, callback) -> SyncFnConfigPlugin<C, F>;
pub fn plugin_async_with_config<C, F>(name, inject, callback) -> AsyncFnConfigPlugin<C, F>;
pub fn service_sync_with_config<S, C, F>(name, inject, constructor) -> SyncServiceConfigPlugin<S, C, F>;
pub fn service_async_with_config<S, C, F>(name, inject, constructor) -> AsyncServiceConfigPlugin<S, C, F>;
```

The `_with_config` helpers receive `Arc<C>` and type-check the config through
`validate_config`.

## Inject and Dependency

`Inject` is the normalized service dependency declaration. Each entry pairs a
required service name with optional intercept config applied to the plugin's
scoped context before its `apply` runs.

```rust
pub struct Dependency {
    pub name: String,
    pub config: Option<Config>,
}

pub struct Inject { /* private */ }

impl Inject {
    pub fn new<I, S>(names: I) -> Self;            // names only
    pub fn none() -> Self;
    pub fn require(self, name) -> Self;            // name without config
    pub fn require_with<T>(self, name, config) -> Self;
    pub fn require_with_value(self, name, config: Config) -> Self;
    pub fn iter(&self) -> impl Iterator<Item = &Dependency>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn contains(&self, name: &str) -> bool;
    pub fn names(&self) -> impl Iterator<Item = &str>;
}

impl<const N: usize> From<[&str; N]> for Inject;
impl From<Vec<String>> for Inject;
```

`require_with` / `require_with_value` mirror Cordis's object-form inject: the
declared config is appended to that service's intercept chain for the plugin
context.

## PluginRegistry

```rust
pub struct PluginRegistry;

impl PluginRegistry {
    pub fn new() -> Self;
    pub fn register(&self, plugin: impl Plugin);
    pub fn register_with_config(&self, plugin: impl Plugin, config: Config);
    pub fn set_apply_timeout(&self, timeout: Duration);
    pub fn set_reconcile_iteration_caps(&self, max_iterations: usize, max_plugin_reconciles: usize);
    pub async fn boot(&self, ctx: &Context, bus: &EventBus) -> anyhow::Result<Vec<PluginHandle>>;
}
```

`boot` resolves `inject` dependencies regardless of registration order, validates
each plugin config, and returns a handle for every started plugin.

`set_reconcile_iteration_caps` overrides the defensive reconcile budget for the
next `boot` (defaults: 64 reconciles per worker wave, 16 per plugin). When a
dynamic dependency cycle that static `provides()` detection cannot see makes a
wave exceed either cap, the worker warns with the plugin name, key, and counts,
abandons the remaining queue, and returns; the next service notification starts
a fresh wave with a full budget. Raise the wave cap for plugin graphs whose
boot cascade legitimately reconciles more than 64 plugins in one wave.

## PluginHandle

```rust
impl PluginHandle {
    pub fn key(&self) -> PluginKey;
    pub fn name(&self) -> &'static str;
    pub fn state(&self) -> FiberState;
    pub async fn shutdown(&self);
    pub async fn update(&self, config: Config) -> anyhow::Result<()>;
}
```

`update` routes the new config through the `internal/update` waterfall, stores
it, and reloads the plugin. Config validation and the `internal/config`
waterfall run again during the reload.

`key()` returns the stable identity assigned to this running instance and can
be passed to `Context::remove_plugin` or `Context::replace_plugin`.

## PluginKey

```rust
pub struct PluginKey(u64);

impl PluginKey {
    pub const fn from_raw(value: u64) -> Self;
    pub const fn as_raw(self) -> u64;
}
```

`PluginKey` identifies one plugin instance. It is allocated per `ctx.plugin`
call, so starting the same plugin type twice yields two distinct keys.
