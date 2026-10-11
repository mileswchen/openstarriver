# Events

插件使用的带类型事件总线。每个事件负载都是一个实现 `Event` 的 Rust 类型。

## 事件契约

```rust
pub trait Event: Send + Sync + 'static {
    const NAME: &'static str;
}

pub trait WaterfallEvent: Event {
    type Output: Send + Sync + 'static + Clone;
}

pub type Next<O> =
    Arc<dyn Fn() -> BoxFuture<'static, Result<O, EventError>> + Send + Sync>;
```

### 事件声明宏

不想手写 struct 和 `Event` 实现时，可以用 `cordis_event!`：

```rust
cordis_rslite::cordis_event!(StatsReport, "stats/report", {
    name: String,
    count: u64,
});
```

这会展开成带有公开 `name`、`count` 字段的 `pub struct StatsReport`，以及
一个 `NAME` 为 `"stats/report"` 的 `Event` 实现。结构体名前可以加可选的
可见性修饰：

```rust
cordis_rslite::cordis_event!(pub(crate) InternalEvent, "internal/event", {});
```

## 错误

```rust
pub enum EventError {
    TypeMismatch(&'static str),
    Cast(ValueCastError),
    Listener(String),
    Timeout,
}
```

## EventBus 构造

```rust
impl EventBus {
    pub fn new() -> Self;
    pub fn with_emit_limit(limit: usize) -> Self;
}
```

## 监听器注册

```rust
pub fn on<E, F>(&self, listener: F) -> EffectHandle
where
    E: Event,
    F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static;

pub fn on_sync<E, F>(&self, listener: F) -> EffectHandle
where
    E: Event,
    F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static;

pub fn on_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
where
    E: Event,
    F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static;

pub fn on_sync_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
where
    E: Event,
    F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static;

pub fn once<E, F>(&self, listener: F) -> EffectHandle
where
    E: Event,
    F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static;

pub fn once_sync<E, F>(&self, listener: F) -> EffectHandle
where
    E: Event,
    F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static;

pub fn once_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
where
    E: Event,
    F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static;

pub fn once_sync_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
where
    E: Event,
    F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static;

// 异步（默认）路径的显式别名。
pub fn on_async<E, F>(&self, listener: F) -> EffectHandle;
pub fn on_async_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle;
pub fn once_async<E, F>(&self, listener: F) -> EffectHandle;
pub fn once_async_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle;

pub fn on_waterfall<E, F>(&self, listener: F) -> EffectHandle
where
    E: WaterfallEvent,
    F: Fn(Arc<E>, Next<E::Output>) -> BoxFuture<'static, Result<E::Output, EventError>>
        + Send + Sync + 'static;
```

所有注册方法都返回 `EffectHandle`，dispose 时会移除监听器。

用 `cordis_event!` 声明事件结构体，用 `event_catalog!` 列出事件：

```rust
cordis_rslite::cordis_event!(StatsReport, "stats/report", {
    name: String,
    count: u64,
});

cordis_rslite::event_catalog!(catalog, {
    "stats/report" => StatsReport,
});
```

### EventOptions

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventOptions {
    pub prepend: bool,
    pub global: bool,
}
```

- `prepend`：将监听器插入到已有监听器之前，而不是之后。
- `global`：在 `*_scoped` 分发中绕过目标 context 过滤。

`once` 监听器在首次真正被调用时移除，而不是在首次被分发时移除。如果前面的监听器
在到达该 `once` 监听器之前 bail 或报错，它仍保持注册，直到真正运行。

## 分发

```rust
pub fn emit<E: Event>(&self, event: Arc<E>);
pub fn emit_sync<E: Event>(&self, event: Arc<E>) -> Vec<Result<Value, EventError>>;

pub async fn serial<E: Event>(&self, event: Arc<E>) -> Result<Option<Value>, EventError>;
pub async fn serial_collect<E: Event>(&self, event: Arc<E>) -> Vec<Result<Value, EventError>>;
pub async fn serial_strict<E: Event>(&self, event: Arc<E>) -> Result<Vec<Value>, EventError>;
pub async fn parallel<E: Event>(&self, event: Arc<E>) -> Result<(), EventError>;
pub async fn bail<E: Event>(&self, event: Arc<E>) -> Result<Option<Value>, EventError>;
pub fn bail_sync<E: Event>(&self, event: Arc<E>) -> Result<Option<Value>, EventError>;

pub async fn waterfall<E, F>(
    &self,
    event: Arc<E>,
    inner: F,
) -> Result<E::Output, EventError>
where
    E: WaterfallEvent,
    F: Fn() -> BoxFuture<'static, Result<E::Output, EventError>> + Send + Sync + 'static;
```

语义：

- `emit`：先立即运行同步监听器，再在受控 in-flight 上限内调度异步监听器。
- `emit_sync`：只运行同步监听器，并返回所有结果。
- `serial`：同步与异步监听器一起按注册顺序执行，返回第一个 `Some(value)`；`Ok(None)` 继续，`Err` 立即失败。
- `serial_collect`：同步与异步监听器一起按序执行，收集所有结果并隔离错误。
- `serial_strict`：同步与异步监听器一起按序执行，且 fail-fast。
- `parallel`：同步与异步监听器一起并发分发，丢弃返回值，并把所有错误聚合为 `EventError::Aggregate`。
- `bail`：同步与异步监听器一起做有序 bail；返回第一个 `Some(value)`，遇到 `Err` 立即失败。
- `bail_sync`：只对同步监听器做有序 bail；返回第一个 `Some(value)`，遇到 `Err` 立即失败。
- `waterfall`：Cordis 风格的带类型 `next()` continuation 链。最内层 `inner` 产生默认 `Output`；每个监听器可以调用 `next()` 运行后续链路并对结果做后处理，也可以不调用 `next()` 直接返回以短路。

## 目标分发与过滤

每种分发模式都有 `*_scoped` 变体，接受目标 `Context`：

```rust
pub fn emit_scoped<E: Event>(&self, target: &Context, event: Arc<E>);
pub fn emit_sync_scoped<E: Event>(&self, target: &Context, event: Arc<E>) -> Vec<Result<Value, EventError>>;
pub async fn serial_scoped<E: Event>(&self, target: &Context, event: Arc<E>) -> Result<Option<Value>, EventError>;
pub async fn serial_collect_scoped<E: Event>(&self, target: &Context, event: Arc<E>) -> Vec<Result<Value, EventError>>;
pub async fn serial_strict_scoped<E: Event>(&self, target: &Context, event: Arc<E>) -> Result<Vec<Value>, EventError>;
pub async fn parallel_scoped<E: Event>(&self, target: &Context, event: Arc<E>) -> Result<(), EventError>;
pub async fn bail_scoped<E: Event>(&self, target: &Context, event: Arc<E>) -> Result<Option<Value>, EventError>;
pub fn bail_sync_scoped<E: Event>(&self, target: &Context, event: Arc<E>) -> Result<Option<Value>, EventError>;
pub async fn waterfall_scoped<E, F>(&self, target: &Context, event: Arc<E>, inner: F) -> Result<E::Output, EventError>;
```

完整变体矩阵：

| 模式 | 全局 | 目标作用域 |
|---|---|---|
| emit | `emit` / `emit_sync` | `emit_scoped` / `emit_sync_scoped` |
| serial | `serial` | `serial_scoped` |
| serial_collect | `serial_collect` | `serial_collect_scoped` |
| serial_strict | `serial_strict` | `serial_strict_scoped` |
| parallel | `parallel` | `parallel_scoped` |
| bail | `bail` / `bail_sync` | `bail_scoped` / `bail_sync_scoped` |
| waterfall | `waterfall` | `waterfall_scoped` |

过滤规则：

- 使用 `EventOptions { global: true }` 注册的监听器始终运行。
- 直接注册在 `EventBus` 上（没有所属 `Context`）的监听器始终运行。
- 否则，当目标携带 `ContextFilter` 时，把过滤器应用到监听器的所属 context；返回 `false` 则跳过该监听器。
- 目标没有过滤器时，所有监听器都运行。

监听器所属 context 来自 `Context` 事件 API（`ctx.on`、`ctx.once`、`ctx.on_waterfall` 等）。`bus.on` 等创建的监听器没有所属 context。

## 动态事件

`bus.dynamic()` 提供基于运行时字符串 key 的 waterfall facade，用于外部插件以及 `internal/get` / `internal/set` 等反射事件：

```rust
impl EventBus {
    pub fn dynamic(&self) -> DynamicEvents;
}

impl DynamicEvents {
    pub fn on_waterfall<F>(&self, name: impl Into<String>, listener: F) -> EffectHandle
    where
        F: Fn(DynamicEvent, DynamicNext) -> BoxFuture<'static, Result<Option<Value>, EventError>>
            + Send + Sync + 'static;

    pub fn waterfall<F>(
        &self,
        name: impl Into<String>,
        args: Vec<Value>,
        inner: F,
    ) -> BoxFuture<'static, Result<Option<Value>, EventError>>
    where
        F: Fn() -> BoxFuture<'static, Result<Option<Value>, EventError>> + Send + Sync + 'static;
}
```

`Option<Value>` 遵循 Cordis 约定：`Some(value)` 表示具体结果或短路，`None` 表示无值或继续到底。监听器不调用 `next()` 直接返回即短路。

## 可观测性

```rust
pub fn dropped_emits(&self) -> u64;
pub fn emit_in_flight(&self) -> usize;
pub fn emit_limit(&self) -> usize;
pub fn listener_count(&self, name: &'static str) -> usize;
pub fn listener_calls(&self) -> u64;
pub fn listener_total_ns(&self) -> u64;
pub fn listener_avg_ns(&self) -> u64;
pub fn dispatch_calls(&self) -> u64;
pub fn dispatch_total_ns(&self) -> u64;
pub fn dispatch_avg_ns(&self) -> u64;
```

`serial` / `bail` 始终 fail-closed；`emit` / `parallel` 保留各自的隔离或严格变体。没有全局分发
策略：失败模式按每个分发方法固定。
