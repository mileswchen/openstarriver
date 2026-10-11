//! Example for tutorial chapter 3: services (service + inject + optional dependencies).
//! Run: `cargo run -p cordis-rslite --example 03_services`

use cordis_rslite::{
    service_sync, Context, EffectHandle, EventBus, Inject, Plugin, PluginRegistry, Service,
    TypedService,
};

struct Greeter;
impl TypedService for Greeter {
    const NAME: &'static str = "greeter";
}
impl Greeter {
    fn greet(&self, who: &str) -> String {
        format!("Hello, {}!", who)
    }
}

struct Consumer;
impl Plugin for Consumer {
    fn name(&self) -> &'static str {
        "consumer"
    }
    fn inject(&self) -> Inject {
        Inject::new(["greeter"])
    }
    fn apply(
        &self,
        ctx: &Context,
        _bus: &EventBus,
    ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let ctx = ctx.clone();
        Box::pin(async move {
            let greeter = ctx.require_named::<Greeter>("greeter")?;
            println!("{}", greeter.greet("world"));
            Ok(vec![])
        })
    }
}

struct OptionalConsumer;
impl Plugin for OptionalConsumer {
    fn name(&self) -> &'static str {
        "optional-consumer"
    }
    // No inject declaration: greeter is an optional dependency.
    fn apply(
        &self,
        ctx: &Context,
        _bus: &EventBus,
    ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let ctx = ctx.clone();
        Box::pin(async move {
            match ctx.require_named::<Greeter>("greeter") {
                Ok(greeter) => println!("optional: {}", greeter.greet("maybe")),
                Err(_) => println!("optional: no greeter available"),
            }
            Ok(vec![])
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();

    registry.register(service_sync::<Greeter, _>(
        "greeter",
        Inject::none(),
        |_ctx| Ok(Greeter),
    ));
    registry.register(Consumer);
    registry.register(OptionalConsumer);

    let _handles = registry.boot(&ctx, &bus).await?;
    Ok(())
}
