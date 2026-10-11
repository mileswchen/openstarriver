use std::sync::Arc;

use cordis_rslite::{
    Context, EffectHandle, Event, EventBus, Plugin, PluginRegistry, TypedService, Value,
};

struct Greeting(Arc<str>);
impl TypedService for Greeting {
    const NAME: &'static str = "greeting";
}

#[derive(Clone)]
struct Greet(String);
impl Event for Greet {
    const NAME: &'static str = "demo/greet";
}

struct HelloPlugin;
impl Plugin for HelloPlugin {
    fn name(&self) -> &'static str {
        "hello"
    }
    fn provides(&self) -> &[&'static str] {
        &["greeting"]
    }
    fn apply(
        &self,
        ctx: &Context,
        bus: &EventBus,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
        let ctx = ctx.clone();
        let bus = bus.clone();
        Box::pin(async move {
            Ok(vec![
                ctx.provide::<Greeting>(Arc::new(Greeting(Arc::from("hello"))))
                    .map_err(anyhow::Error::from)?,
                bus.on::<Greet, _>(|e| {
                    let msg = e.0.clone();
                    Box::pin(async move { Ok(Value::new(msg)) })
                }),
            ])
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    registry.register(HelloPlugin);
    let _handles = registry.boot(&ctx, &bus).await?;

    let greeting = ctx.require::<Greeting>()?;
    println!("service: {}", greeting.0);

    let results = bus.serial_collect(Arc::new(Greet("world".into()))).await;
    for r in results {
        let v = r?;
        println!("event: {}", v.downcast::<String>().unwrap());
    }
    Ok(())
}
