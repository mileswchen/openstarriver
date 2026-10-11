//! Example for tutorial chapter 6: dynamic registry, isolation, and filtered
//! events.
//! Run: `cargo run -p cordis-rslite --example 06_dynamic_and_scopes`

use std::sync::Arc;

use cordis_rslite::{
    service_sync, Context, Event, EventBus, Inject, PluginRegistry, Service, TypedService, Value,
};

struct Greeting(Arc<str>);

impl TypedService for Greeting {
    const NAME: &'static str = "greeting";
}

#[derive(Clone)]
struct Ping;

impl Event for Ping {
    const NAME: &'static str = "ping";
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();

    // Boot an empty registry so `ctx.plugin` has a runtime root to attach to.
    let _ = registry.boot(&ctx, &bus).await?;

    // 1. Dynamic registry: mount, replace, and remove a service provider at
    // runtime.
    let base = ctx
        .plugin(service_sync::<Greeting, _>(
            "greeting",
            Inject::none(),
            |_ctx| Ok(Greeting(Arc::from("base"))),
        ))
        .await?;
    println!("greeting after add: {}", ctx.require::<Greeting>()?.0);

    let replacement = ctx
        .replace_plugin(
            base.key(),
            service_sync::<Greeting, _>("greeting-v2", Inject::none(), |_ctx| {
                Ok(Greeting(Arc::from("replaced")))
            }),
        )
        .await?;
    println!("greeting after replace: {}", ctx.require::<Greeting>()?.0);

    // 2. Isolation: a fresh, independent slot for `greeting` in a child
    // context. The child does not inherit the root implementation.
    let child = ctx.isolate("greeting");
    let _isolated_effect = child.provide::<Greeting>(Arc::new(Greeting(Arc::from("isolated"))))?;
    println!("root greeting: {}", ctx.require::<Greeting>()?.0);
    println!("isolated greeting: {}", child.require::<Greeting>()?.0);

    // 3. Filtered events: an owned listener is subject to the target context
    // filter, while an owner-less listener always runs. We use the synchronous
    // path here so the output order is deterministic.
    let owner = Context::new().with_event_bus(&bus);
    let _owned = owner.on_sync::<Ping, _>(|_event| {
        println!("owned listener ran");
        Ok(Value::new(()))
    });
    let _global = bus.on_sync::<Ping, _>(|_event| {
        println!("global listener ran");
        Ok(Value::new(()))
    });

    let allow = Context::new().with_filter(|_ctx| true);
    let deny = Context::new().with_filter(|_ctx| false);

    println!("--- allow target ---");
    bus.emit_sync_scoped(&allow, Arc::new(Ping));
    println!("--- deny target ---");
    bus.emit_sync_scoped(&deny, Arc::new(Ping));

    // Teardown: removing the replacement also removes its provided service.
    assert!(ctx.remove_plugin(replacement.key()).await);
    assert!(!ctx.has("greeting"));

    Ok(())
}
