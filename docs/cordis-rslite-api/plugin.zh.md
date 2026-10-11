# Plugin 与 Registry

插件入口形态与依赖感知启动。

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

需要配置的插件覆盖 `validate_config` 做类型检查/校验，并覆盖
`apply_with_config` 消费校验后的配置。旧插件只实现 `apply(ctx, bus)` 即可；
registry 始终通过 `apply_with_config` 调用，其默认实现会委托给 `apply`。

## 内置插件形态

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

`_with_config` 系列 helper 接收 `Arc<C>`，并在 `validate_config` 中做类型检查。

## Inject 与 Dependency

`Inject` 是标准化后的服务依赖声明。每个条目把“所需服务名”与“可选的拦截配置”配对；该配置会在插件 `apply` 执行前挂到插件的 scoped context 上。

```rust
pub struct Dependency {
    pub name: String,
    pub config: Option<Config>,
}

pub struct Inject { /* private */ }

impl Inject {
    pub fn new<I, S>(names: I) -> Self;            // 仅服务名
    pub fn none() -> Self;
    pub fn require(self, name) -> Self;            // 无配置的服务名
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

`require_with` / `require_with_value` 对应 Cordis 的对象形式 inject：声明的配置会被追加到该服务在插件上下文中的拦截链。

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

`boot` 无论注册顺序如何，都会解析 `inject` 依赖、校验每个插件配置，并为每个已启动的插件返回 handle。

`set_reconcile_iteration_caps` 覆盖下一次 `boot` 的防御性 reconcile 预算（默认：每个 worker 波形 64 次、每插件 16 次）。当静态 `provides()` 检测不到的动态依赖环使某波形超出任一上限时，worker 会带插件名、key 与计数告警，丢弃剩余队列并返回；下一次服务通知会以全新预算开启新波形。若插件图的 boot 级联在单波形内合法超过 64 次 reconcile，应调大 wave 上限。

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

`update` 会把新配置经过 `internal/update` waterfall、保存后重载插件。重载期间会再次执行配置校验与 `internal/config` waterfall。

`key()` 返回当前运行实例的稳定身份，可传给 `Context::remove_plugin` 或 `Context::replace_plugin`。

## PluginKey

```rust
pub struct PluginKey(u64);

impl PluginKey {
    pub const fn from_raw(value: u64) -> Self;
    pub const fn as_raw(self) -> u64;
}
```

`PluginKey` 标识一个插件实例。每次 `ctx.plugin` 调用都会分配一个新 key，因此重复启动同一插件类型会得到两个不同的 key。
