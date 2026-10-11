# Context

插件使用的根依赖容器与子依赖容器。

## 类型

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

## 构造与作用域

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

`isolate(name)` 为某个服务名创建全新的独立实现槽，槽初始为空而不是继承父值。
`isolate_with(name, label)` 复用显式 label，使不同 context 分支加入同一槽位。
`isolate_many(names, label?)` 把多个服务名映射到同一个共享槽位。
`new_isolation` 分配一个唯一 label，不影响 context。
`with_event_bus` 挂载事件总线，使异步服务访问器可以分发 `internal/get` 和 `internal/set`。

`intercept` 返回一个携带 `name` 额外拦截配置的子 context，父 context 不受影响。拦截配置按“根优先”顺序累积，可通过 `intercepts(name)` 读取。

`with_filter` 为 `*_scoped` 分发挂载事件监听器过滤器：所属 context 未通过过滤器的监听器会被跳过。`filter` 返回已挂载的谓词（如果有）。

## 事件

`Context` 是主要的事件 API。监听器注册会把 context 记录为所属者，以便目标过滤分发判断该监听器是否应运行：

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

这些方法需要已挂载的事件总线；请先调用 `with_event_bus`（或启动 `PluginRegistry`）。不带显式目标的分发会运行所有监听器；使用 `EventBus::*_scoped` 可按目标 context 过滤。

## Effect 与所有权

`Context::effect` 在 context 分支上注册清理动作，由 `Context::dispose_effects` 统一执行。监听器注册（`on*` / `once*` / `on_waterfall`）走同一套跟踪。

所有权规则（caller-fiber 语义）：当 context 作用域挂在某个 provider fiber 上时——例如插件 `apply` 内拿到的 context——通过该 context 注册的每个 effect（监听器、清理动作、提供的服务）都会被注册方 fiber 认领。注册方重载或销毁时，恰好清理它自己注册的 effect。被依赖的 provider 失活不会清理依赖方的 effect；依赖方通过 reconcile 自行重估（re-apply 或卸载时清理自己的 fiber）。没有当前 fiber 的 context（根、isolate、tagged 作用域）保持分支作用域的 `dispose_effects` 销毁方式。

## 插件

```rust
pub fn plugin(&self, child: impl Plugin) -> BoxFuture<anyhow::Result<PluginHandle>>;
pub fn plugin_with_config(&self, child: impl Plugin, config: Config) -> BoxFuture<anyhow::Result<PluginHandle>>;
pub fn remove_plugin(&self, key: PluginKey) -> BoxFuture<bool>;
pub fn replace_plugin(&self, old: PluginKey, new: impl Plugin) -> BoxFuture<anyhow::Result<PluginHandle>>;
pub fn replace_plugin_with_config(&self, old: PluginKey, new: impl Plugin, config: Config) -> BoxFuture<anyhow::Result<PluginHandle>>;
```

在挂载到该 context 的运行时 registry 上启动插件。存在当前 fiber 时，插件会作为其子级挂载，并随父级一起销毁；否则作为顶层运行时条目启动。`plugin` 默认配置为 `()`。

`remove_plugin(key)` 按稳定 key 销毁某个运行实例，找不到时返回 `false`。`replace_plugin` / `replace_plugin_with_config` 先移除 `old` 实例，再在相同归属位置启动新插件。

这些方法要求 context 根部已挂载 registry。`PluginRegistry::boot` 会挂载 registry；未 boot 的手工 context 会回退到仅支持 `Spawner` 的子插件挂载路径，该路径仍要求当前 fiber 存在。

## 服务

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

当存在当前 fiber 时，`provide` 的注册会自动附着到该 fiber；否则返回的 `EffectHandle` 需要手动 dispose。在同一个作用域内再次提供同名服务会返回 `Err(ContextError::Duplicate)`；请先 dispose 旧 handle（或移除所属插件）再重新提供。

`provide_any` / `require_any` 是面向外部插件的无类型、动态名字逃生通道：它们直接携带 `Value`，不要求具体类型实现 `Service`；调用方自行对结果做 downcast。

`provide_dyn` / `require_dyn` 是 `Arc<dyn Service>` 的对象安全通道,以显式运行时名提供与解析,并保留 `is_available` 门控。零参 `require::<T>()` 无法作用于 dyn 服务(没有实例、也没有类型级名称),dyn 访问一律走显式名。

当挂载了事件总线时，`*_async` 访问器会走动态 waterfall：

- `require_named_async` 分发 `internal/get`；监听器可以在不调用 `next()` 的情况下直接返回 `Some(value)` 来短路。
- `provide_named_async` 分发 `internal/set`；监听器可以在不调用 `next()` 的情况下返回 `Some(false)` 来否决写入。

未挂载总线时，异步访问器回退到同步实现。

## 变更通知

```rust
pub fn subscribe(&self, callback: Subscriber) -> EffectHandle;
```

## 公开 seam 键

```rust
pub const KERNEL_PREFIX: &str = "kernel:";
pub const SEAM_PREFIX: &str = "seam:";
pub fn is_public_seam(name: &str) -> bool;
```
