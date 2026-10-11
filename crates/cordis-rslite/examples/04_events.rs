//! Example for tutorial chapter 4: events (typed payloads, emit, waterfall).
//! Run: `cargo run -p cordis-rslite --example 04_events`

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cordis_rslite::{
    service_sync, Context, EffectHandle, Event, EventBus, Inject, Plugin, PluginRegistry, Service,
    TypedService, Value, WaterfallEvent,
};

/// Payload for `stats/report`.
struct StatsReport {
    name: String,
    count: u64,
}

impl Event for StatsReport {
    const NAME: &'static str = "stats/report";
}

/// A service that owns its state and announces changes through the event bus.
struct Stats {
    bus: EventBus,
    counts: Mutex<HashMap<String, u64>>,
}

impl TypedService for Stats {
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
        let _ = self.bus.emit_sync(Arc::new(StatsReport {
            name: name.to_owned(),
            count: next,
        }));
    }
}

/// Declares the `stats/report` event and receives emitted notifications.
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

/// Payload for `demo/transform`.
struct DemoTransform {
    input: String,
}

impl Event for DemoTransform {
    const NAME: &'static str = "demo/transform";
}

impl WaterfallEvent for DemoTransform {
    type Output = String;
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();

    let stats_bus = bus.clone();
    registry.register(service_sync::<Stats, _>(
        "stats",
        Inject::none(),
        move |_ctx| Ok(Stats::new(stats_bus.clone())),
    ));
    registry.register(Reporter);

    let _handles = registry.boot(&ctx, &bus).await?;

    // The first listener short-circuits when it owns the decision.
    let _short_circuit = bus.on_waterfall::<DemoTransform, _>(|event, next| {
        Box::pin(async move {
            if event.input.contains("blocked") {
                Ok("** blocked **".to_owned())
            } else {
                next().await
            }
        })
    });

    let _uppercase = bus.on_waterfall::<DemoTransform, _>(|_event, next| {
        Box::pin(async move {
            let downstream = next().await?;
            Ok(downstream.to_uppercase())
        })
    });

    let hello = bus
        .waterfall(
            Arc::new(DemoTransform {
                input: "hello".into(),
            }),
            || Box::pin(async { Ok("hello".to_owned()) }),
        )
        .await?;
    println!("{hello}");

    let blocked = bus
        .waterfall(
            Arc::new(DemoTransform {
                input: "blocked words".into(),
            }),
            || Box::pin(async { Ok("blocked words".to_owned()) }),
        )
        .await?;
    println!("{blocked}");

    Ok(())
}
