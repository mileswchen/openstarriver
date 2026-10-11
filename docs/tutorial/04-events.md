# 4. Events

English | [中文](04-events.zh.md)

Services support direct calls; **events** let a plugin announce something without knowing which plugins listen. Starriver uses a shared `EventBus` for this, so producers and listeners are decoupled.

## Declare, emit, listen

Events are typed Rust values, not TypeScript string declarations. To create an event you define a struct and implement `Event`, which gives it a stable runtime name:

```rust
use cordis_rslite::Event;

struct StatsReport {
    name: String,
    count: u64,
}

impl Event for StatsReport {
    const NAME: &'static str = "stats/report";
}
```

The `namespace/action` naming convention keeps the flat event namespace readable. Unlike a TypeScript `interface Events` merge, the payload is always a single `Arc<StatsReport>`; there are no variadic event arguments.

Next, create a service that owns state and announces each change:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cordis_rslite::{EventBus, Service};

struct Stats {
    bus: EventBus,
    counts: Mutex<HashMap<String, u64>>,
}

impl Service for Stats {
    const NAME: &'static str = "stats";
}

impl Stats {
    fn new(bus: EventBus) -> Self {
        Self {
            bus,
            counts: Mutex::new(HashMap::new()),
        }
    }

    fn bump(&self, name: &str) {
        let next = {
            let mut counts = self.counts.lock().expect("stats counts poisoned");
            let next = counts.get(name).copied().unwrap_or(0) + 1;
            counts.insert(name.to_owned(), next);
            next
        };
        let _ = self
            .bus
            .emit_sync(Arc::new(StatsReport { name: name.to_owned(), count: next }));
    }
}
```

Register a synchronous listener with `bus.on_sync::<E, _>(...)`. The call returns an `EffectHandle`; return it from `apply` so the listener disappears with the plugin:

```rust
use cordis_rslite::{Context, EffectHandle, EventBus, Inject, Plugin, Value};

struct Reporter;

impl Plugin for Reporter {
    fn name(&self) -> &'static str {
        "reporter"
    }

    fn inject(&self) -> Inject {
        Inject::new(["stats"])
    }

    fn apply(
        &self,
        ctx: &Context,
        bus: &EventBus,
    ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let ctx = ctx.clone();
        let bus = bus.clone();
        Box::pin(async move {
            let stats = ctx.require_named::<Stats>("stats")?;
            let report_effect = bus.on_sync::<StatsReport, _>(|event| {
                println!("[stats] {} -> {}", event.name, event.count);
                Ok(Value::new(()))
            });

            stats.bump("tool_call");
            stats.bump("tool_call");
            stats.bump("prompt");

            Ok(vec![report_effect])
        })
    }
}
```

Composing these two plugins prints:

```
[stats] tool_call -> 1
[stats] tool_call -> 2
[stats] prompt -> 1
```

Because `bus.on_sync` is an effect, the listener is removed automatically when the plugin is unloaded — no manual deregistration bookkeeping.

## Dispatch modes

`emit` is one of several dispatch modes. The mode is part of an event's contract: it decides whether listeners can return values, run concurrently, or short-circuit each other.

| Mode | Call | Semantics |
|---|---|---|
| emit | `bus.emit(Arc<E>)` | Runs synchronous listeners immediately in registration order, then schedules asynchronous listeners under a bounded in-flight limit. Return values are not collected; an asynchronous listener invocation may be dropped when the limit is exhausted. |
| emit_sync | `bus.emit_sync(Arc<E>)` | Runs only synchronous listeners immediately and returns `Vec<Result<Value, EventError>>`. |
| parallel | `bus.parallel(Arc<E>).await` | Runs all listeners concurrently, discards values, and aggregates errors into `EventError::Aggregate`. |
| serial | `bus.serial(Arc<E>).await` | Both synchronous and asynchronous listeners run in registration order and return `Result<Option<Value>, EventError>`. The first `Some(value)` wins and stops the chain; `Ok(None)` continues; `Err` fails closed. |
| serial_collect | `bus.serial_collect(Arc<E>).await` | Both listener kinds run in registration order and collect every result with isolated errors. |
| serial_strict | `bus.serial_strict(Arc<E>).await` | Both listener kinds run in registration order and dispatch returns `Err` on the first failing listener. |
| bail | `bus.bail(Arc<E>).await` | Runs both synchronous and asynchronous listeners in order and returns `Result<Option<Value>, EventError>`; the first `Some(value)` stops the chain. |
| bail_sync | `bus.bail_sync(Arc<E>)` | Runs synchronous listeners in order and returns `Result<Option<Value>, EventError>`; the first `Some(value)` stops the chain. |
| waterfall | `bus.waterfall(Arc<E>, inner).await` | A typed `next()` continuation chain; each listener can call `next()` to run downstream listeners and post-process the result, or return without calling `next()` to short-circuit. |

The failure mode is fixed per dispatch method: `serial` / `bail` are always fail-closed, and
`emit` / `parallel` keep their isolated or strict variants. There is no global dispatch policy.

## Waterfall: transform or short-circuit

Waterfall is Starriver's interception primitive. Each listener receives the typed event and a `next()` continuation. Calling `next()` runs the rest of the chain; returning without calling it short-circuits.

```rust
use cordis_rslite::{Event, EventBus, WaterfallEvent};

struct DemoTransform {
    input: String,
}

impl Event for DemoTransform {
    const NAME: &'static str = "demo/transform";
}

impl WaterfallEvent for DemoTransform {
    type Output = String;
}

// Short-circuit only when this listener owns the decision; otherwise pass through.
bus.on_waterfall::<DemoTransform, _>(|event, next| {
    Box::pin(async move {
        if event.input.contains("blocked") {
            Ok("** blocked **".to_owned())
        } else {
            next().await
        }
    })
});

// Post-process the downstream result.
bus.on_waterfall::<DemoTransform, _>(|_event, next| {
    Box::pin(async move {
        let downstream = next().await?;
        Ok(downstream.to_uppercase())
    })
});

let hello = bus
    .waterfall(
        std::sync::Arc::new(DemoTransform { input: "hello".into() }),
        || Box::pin(async { Ok("hello".to_owned()) }),
    )
    .await?;
let blocked = bus
    .waterfall(
        std::sync::Arc::new(DemoTransform { input: "blocked words".into() }),
        || Box::pin(async { Ok("blocked words".to_owned()) }),
    )
    .await?;
```

Running the complete example prints:

```
HELLO
** blocked **
```

`on_waterfall` provides the typed Cordis-style `next()` continuation. A listener that calls `next().await` can post-process the downstream result; returning without calling `next()` short-circuits the chain.

Run the ready-made example:

```sh
cargo run -p cordis-rslite --example 04_events
```

> **Differences from Cordis:** events are typed Rust types implementing `Event`, not a TypeScript declaration merge. `emit` runs synchronous listeners inline and asynchronous listeners as best-effort tasks, so asynchronous listener invocations can be dropped under overload; use `emit_sync` with `on_sync` for deterministic delivery. `serial` returns the first `Some(value)` and fails closed, while `serial_collect` keeps the old collect-all behavior; `parallel` aggregates errors like Cordis; `bail` and `bail_sync` are the asynchronous/synchronous bail paths. Typed waterfalls use `on_waterfall` / `waterfall`; dynamic waterfalls use `bus.dynamic()`. `serial_strict` and dispatch metrics are additions beyond the Cordis tutorial.

Next: [Configuration](05-config.md) — plugin options from configuration.
