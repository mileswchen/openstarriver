//! Example for tutorial chapter 1: the three plugin shapes.
//! Run: `cargo run -p cordis-rslite --example 01_first_plugin`

use cordis_rslite::{
    plugin_async, plugin_sync, service_sync, Context, EffectHandle, EventBus, Inject, Plugin,
    PluginRegistry, Service, TypedService,
};

struct Greeting;
impl TypedService for Greeting {
    const NAME: &'static str = "greeting";
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();

    // 1. Function plugin (sync)
    registry.register(plugin_sync("hello", Inject::none(), |_ctx, _bus| {
        println!("hello from my first plugin");
        Ok(vec![])
    }));

    // 1b. Function plugin (async)
    registry.register(plugin_async("hello-async", Inject::none(), |_ctx, _bus| {
        Box::pin(async move {
            println!("hello from async plugin");
            Ok(vec![])
        })
    }));

    // 2. Object plugin
    struct ObjectPlugin;
    impl Plugin for ObjectPlugin {
        fn name(&self) -> &'static str {
            "object"
        }
        fn apply(
            &self,
            _ctx: &Context,
            _bus: &EventBus,
        ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
            Box::pin(async {
                println!("hello from object plugin");
                Ok(vec![])
            })
        }
    }
    registry.register(ObjectPlugin);

    // 3. Service plugin
    registry.register(service_sync::<Greeting, _>(
        "service",
        Inject::none(),
        |_ctx| Ok(Greeting),
    ));

    let handles = registry.boot(&ctx, &bus).await?;
    println!("booted {} plugin(s)", handles.len());
    let _greeting = ctx.require::<Greeting>()?;
    println!("greeting service available");
    Ok(())
}
