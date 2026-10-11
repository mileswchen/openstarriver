# Events

Typed event bus used by plugins. Every event payload is a Rust type implementing `Event`.

## Event contract

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

### Event declaration macro

Instead of hand-writing a struct and an `Event` implementation, use
`cordis_event!`:

```rust
cordis_rslite::cordis_event!(StatsReport, "stats/report", {
    name: String,
    count: u64,
});
```

This expands to a `pub struct StatsReport` with public `name` and `count`
fields and an `Event` implementation whose `NAME` is `"stats/report"`. An
optional visibility qualifier may precede the struct name:

```rust
cordis_rslite::cordis_event!(pub(crate) InternalEvent, "internal/event", {});
```

## Errors

```rust
pub enum EventError {
    TypeMismatch(&'static str),
    Cast(ValueCastError),
    Listener(String),
    Timeout,
}
```

## EventBus construction

```rust
impl EventBus {
    pub fn new() -> Self;
    pub fn with_emit_limit(limit: usize) -> Self;
}
```

## Listener registration

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

// Explicit aliases for the async (default) path.
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

All registration methods return an `EffectHandle` that removes the listener when disposed.

Use `cordis_event!` to declare event structs and `event_catalog!` to list them:

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

- `prepend`: insert the listener before existing listeners instead of after.
- `global`: bypasses target-context filtering for `*_scoped` dispatch.

`once` listeners are removed when first invoked, not when first dispatched. If an earlier
listener bails or errors before the `once` listener is reached, the `once` listener stays
registered until it actually runs.

## Dispatch

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

Semantics:

- `emit`: runs synchronous listeners immediately, then schedules asynchronous listeners under a bounded in-flight limit.
- `emit_sync`: runs only synchronous listeners and returns every result.
- `serial`: ordered over both synchronous and asynchronous listeners; returns the first
  `Some(value)`; `Ok(None)` continues, `Err` fails closed.
- `serial_collect`: ordered over both listener kinds, collects every result with isolated errors.
- `serial_strict`: ordered over both listener kinds and fail-fast.
- `parallel`: concurrent dispatch over both listener kinds; discards values and aggregates
  all listener errors into `EventError::Aggregate`.
- `bail`: ordered bail over both synchronous and asynchronous listeners; returns the first
  `Some(value)` and fails closed on `Err`.
- `bail_sync`: ordered bail over synchronous listeners only; returns the first `Some(value)` and
  fails closed on `Err`.
- `waterfall`: a typed Cordis-style `next()` continuation chain. The innermost `inner` produces the default `Output`; each listener can call `next()` to run the rest of the chain and post-process the result, or return without calling `next()` to short-circuit.

## Target dispatch and filtering

Every dispatch mode has a `*_scoped` variant that accepts a target `Context`:

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

The full variant matrix is:

| mode | global | target-scoped |
|---|---|---|
| emit | `emit` / `emit_sync` | `emit_scoped` / `emit_sync_scoped` |
| serial | `serial` | `serial_scoped` |
| serial_collect | `serial_collect` | `serial_collect_scoped` |
| serial_strict | `serial_strict` | `serial_strict_scoped` |
| parallel | `parallel` | `parallel_scoped` |
| bail | `bail` / `bail_sync` | `bail_scoped` / `bail_sync_scoped` |
| waterfall | `waterfall` | `waterfall_scoped` |

Filtering rule:

- A listener registered with `EventOptions { global: true }` always runs.
- A listener registered directly on `EventBus` (no owning `Context`) always runs.
- Otherwise, when the target carries a `ContextFilter`, the filter is applied to
  the listener's owning context; a `false` result skips the listener.
- When the target has no filter, every listener runs.

Listener owners come from the `Context` event API (`ctx.on`, `ctx.once`,
`ctx.on_waterfall`, ...). `bus.on` and friends create owner-less listeners.

## Dynamic events

`bus.dynamic()` exposes a runtime-string-keyed waterfall facade for external plugins and reflection events such as `internal/get` / `internal/set`:

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

`Option<Value>` follows the Cordis convention: `Some(value)` is a concrete result or short-circuit, and `None` means no value or continue to the end. A listener that returns without calling `next()` short-circuits the chain.

## Observability

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

`serial` / `bail` are always fail-closed; `emit` / `parallel` keep their isolated or strict
variants. There is no global dispatch policy: the failure mode is fixed per dispatch method.
