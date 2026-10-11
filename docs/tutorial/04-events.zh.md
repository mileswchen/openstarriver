# 4. 事件

[English](04-events.md) | 中文

服务支持直接调用；**事件**让插件无需知道有哪些插件正在监听，就能发出通知。Starriver 使用共享的 `EventBus` 来实现这一点，从而让生产方与监听方彼此解耦。

## 声明、发出与监听

事件是带类型的 Rust 值，而不是 TypeScript 的字符串声明。要创建事件，你需要定义一个结构体并实现 `Event`，从而为它赋予稳定的运行时名称：

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

`namespace/action` 命名约定让扁平的事件命名空间保持易读。与 TypeScript 的 `interface Events` 合并不同，这里的负载始终是单个 `Arc<StatsReport>`，没有可变数量的事件参数。

接下来，创建一个负责持有状态并在每次变化时发出通知的服务：

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

使用 `bus.on_sync::<E, _>(...)` 注册同步监听器。这个调用会返回 `EffectHandle`；将它从 `apply` 返回，监听器就会随插件一同消失：

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

组合并运行这两个插件会打印：

```
[stats] tool_call -> 1
[stats] tool_call -> 2
[stats] prompt -> 1
```

因为 `bus.on_sync` 属于 effect，插件卸载时会自动移除监听器，无需手动维护取消注册。

## 分发模式

`emit` 是多种分发模式之一。模式是事件约定的一部分：它决定监听器能否返回值、能否并发运行，以及能否彼此短路。

| 模式 | 调用 | 语义 |
|---|---|---|
| emit | `bus.emit(Arc<E>)` | 先按注册顺序立即运行同步监听器，再在受控的 in-flight 上限内调度异步监听器。不会收集返回值；异步监听器调用在超过上限时可能被丢弃。 |
| emit_sync | `bus.emit_sync(Arc<E>)` | 只立即运行同步监听器，并返回 `Vec<Result<Value, EventError>>`。 |
| parallel | `bus.parallel(Arc<E>).await` | 并发运行所有监听器，丢弃返回值，并把错误聚合为 `EventError::Aggregate`。 |
| serial | `bus.serial(Arc<E>).await` | 同步与异步监听器按注册顺序运行，返回 `Result<Option<Value>, EventError>`。第一个 `Some(value)` 胜出并停止；`Ok(None)` 继续；`Err` 立即失败。 |
| serial_collect | `bus.serial_collect(Arc<E>).await` | 两类监听器按注册顺序运行，收集所有结果并隔离错误。 |
| serial_strict | `bus.serial_strict(Arc<E>).await` | 两类监听器按注册顺序运行，遇到第一个失败监听器时整个分发返回 `Err`。 |
| bail | `bus.bail(Arc<E>).await` | 按顺序运行同步与异步监听器，返回 `Result<Option<Value>, EventError>`；第一个 `Some(value)` 停止链路。 |
| bail_sync | `bus.bail_sync(Arc<E>)` | 按顺序运行同步监听器，返回 `Result<Option<Value>, EventError>`；第一个 `Some(value)` 停止链路。 |
| waterfall | `bus.waterfall(Arc<E>, inner).await` | 带类型的 `next()` continuation 链；每个监听器可以调用 `next()` 运行下游监听器并对结果做后处理，也可以不调用 `next()` 直接返回以短路。 |

失败模式按分发方法固定：`serial` / `bail` 始终 fail-closed；`emit` / `parallel` 保留各自的隔离或
严格变体。没有全局分发策略。

## waterfall：转换或短路

waterfall 是 Starriver 实现拦截的机制。每个监听器都会收到带类型的事件和 `next()` continuation。调用 `next()` 会执行后续链路；不调用直接返回则会短路。

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

// 仅在该监听器拥有决定权时短路；否则向下传递。
bus.on_waterfall::<DemoTransform, _>(|event, next| {
    Box::pin(async move {
        if event.input.contains("blocked") {
            Ok("** blocked **".to_owned())
        } else {
            next().await
        }
    })
});

// 后处理下游结果。
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

运行完整示例会打印：

```
HELLO
** blocked **
```

`on_waterfall` 提供 Cordis 风格的 typed `next()` continuation。调用 `next().await` 的监听器可以后处理下游结果；不调用 `next()` 直接返回则短路。

运行现成的 example：

```sh
cargo run -p cordis-rslite --example 04_events
```

> **与 Cordis 的差异：** 事件是实现 `Event` 的带类型 Rust 类型，而不是 TypeScript 声明合并。`emit` 会同步执行同步监听器，并将异步监听器作为 best-effort 任务调度，因此异步监听器在过载时仍可能被丢弃；需要确定性投递时请使用 `on_sync` + `emit_sync`。`serial` 返回第一个 `Some(value)` 并 fail-closed，`serial_collect` 保留旧的收集全部行为；`parallel` 与 Cordis 一样聚合错误；`bail` / `bail_sync` 分别是异步 / 同步 bail 路径。typed waterfall 使用 `on_waterfall` / `waterfall`；dynamic waterfall 使用 `bus.dynamic()`。`serial_strict` 以及分发指标是本教程章节之外新增的能力。

下一章：[配置](05-config.zh.md)：来自配置的插件选项。
