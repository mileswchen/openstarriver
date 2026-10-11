//! Example for tutorial chapter 2: lifecycle and effects.
//! Run: `cargo run -p cordis-rslite --example 02_lifecycle`

use std::sync::Arc;

use cordis_rslite::{Context, EffectHandle, EventBus, Plugin, PluginRegistry, TypedService};

struct Greeting(Arc<str>);
impl TypedService for Greeting {
    const NAME: &'static str = "greeting";
}

struct HeartbeatPlugin;
impl Plugin for HeartbeatPlugin {
    fn name(&self) -> &'static str {
        "heartbeat"
    }
    fn apply(
        &self,
        ctx: &Context,
        _bus: &EventBus,
    ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let ctx = ctx.clone();
        Box::pin(async move {
            println!("heartbeat plugin loading");

            let service = ctx
                .provide::<Greeting>(Arc::new(Greeting(Arc::from("hello"))))
                .map_err(anyhow::Error::from)?;
            let sync_effect = EffectHandle::new(|| println!("sync cleanup"));
            let async_effect = EffectHandle::new_async(|| {
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    println!("async cleanup");
                })
            });

            Ok(vec![service, sync_effect, async_effect])
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    registry.register(HeartbeatPlugin);
    let handles = registry.boot(&ctx, &bus).await?;

    println!("greeting: {}", ctx.require::<Greeting>()?.0);
    println!("service present before shutdown: {}", ctx.has("greeting"));

    handles[0].shutdown().await;
    println!("service present after shutdown: {}", ctx.has("greeting"));
    Ok(())
}
