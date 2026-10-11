//! Example for tutorial chapter 5: plugin config, schema validation, intercept,
//! and hot reload.
//! Run: `cargo run -p cordis-rslite --example 05_config`

use std::sync::Arc;

use cordis_rslite::{
    plugin_sync_with_config, service_sync_with_config, Config, Context, EffectHandle, EventBus,
    Inject, PluginRegistry, Service, TypedService, Value,
};

struct Greeter {
    prefix: Arc<str>,
}

impl TypedService for Greeter {
    const NAME: &'static str = "greeter";
}

impl Greeter {
    fn greet(&self, who: &str) -> String {
        format!("{}{}", self.prefix, who)
    }
}

#[derive(Clone)]
struct GreeterConfig {
    prefix: Arc<str>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();

    // 1. Service plugin constructed from validated typed config.
    registry.register_with_config(
        service_sync_with_config::<Greeter, GreeterConfig, _>(
            "greeter",
            Inject::none(),
            |_ctx, config| {
                Ok(Greeter {
                    prefix: config.prefix.clone(),
                })
            },
        ),
        Config::new(GreeterConfig {
            prefix: Arc::from("Hello, "),
        }),
    );

    // 2. Function plugin that reads config and provides a runtime value.
    registry.register_with_config(
        plugin_sync_with_config::<u64, _>("counter", Inject::none(), |ctx, _bus, config| {
            ctx.provide_any("count", Value::new(*config))
                .map(|handle| vec![handle])
                .map_err(anyhow::Error::from)
        }),
        Config::new(1u64),
    );

    let handles = registry.boot(&ctx, &bus).await?;

    let greeter = ctx.require::<Greeter>()?;
    println!("{}", greeter.greet("world"));
    println!(
        "count: {}",
        ctx.require_any("count").unwrap().downcast::<u64>().unwrap()
    );

    // 3. Hot-reload the counter plugin with a new config.
    let counter = handles
        .iter()
        .find(|handle| handle.name() == "counter")
        .expect("counter handle");
    counter.update(Config::new(5u64)).await?;
    println!(
        "count after update: {}",
        ctx.require_any("count").unwrap().downcast::<u64>().unwrap()
    );

    // 4. Intercept config is accumulated on a child context without mutating
    // the parent; services can resolve it with `ctx.intercepts(name)`.
    let child = ctx.intercept_typed("greeter", "Configured ");
    println!("intercepts: {}", child.intercepts("greeter").len());

    let _ = EffectHandle::none();
    Ok(())
}
