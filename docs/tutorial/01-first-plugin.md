# 1. Your first plugin

English | [中文](01-first-plugin.zh.md)

Starriver plugins contribute services and event listeners through a **context** (`ctx`) and an **event bus** (`bus`). The kernel only composes plugins; it contains no product logic.

> **Difference from Cordis:** Starriver does not yet have a `cordis.yml` config-file loader. Applications are composed in code through `PluginRegistry` for now. A config loader is planned in a later phase.

## Write the plugin

Create a small binary crate that depends on `cordis-rslite`, and write `main.rs`:

```rust
use cordis_rslite::{plugin_sync, Context, EventBus, Inject, PluginRegistry};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();

    registry.register(plugin_sync("hello", Inject::none(), |_ctx, _bus| {
        println!("hello from my first plugin");
        Ok(vec![])
    }));

    let _handles = registry.boot(&ctx, &bus).await?;
    Ok(())
}
```

The `"hello"` argument is display metadata; the closure is the plugin's `apply` body. The returned `Vec<EffectHandle>` is empty here, because this plugin contributes no services or listeners yet.

## Compose the app

Composition happens in code:

```rust
let registry = PluginRegistry::new();
registry.register(plugin_sync("hello", Inject::none(), |_ctx, _bus| { Ok(vec![]) }));
registry.register(other_plugin);

let handles = registry.boot(&ctx, &bus).await?;
```

Registration order does not decide activation order. Plugins are reconciled by their `inject` dependencies (chapter 3), so a consumer can be registered before its provider.

## Run it

```sh
cargo run
```

Or run the ready-made example in this repository:

```sh
cargo run -p cordis-rslite --example 01_first_plugin
```

Expected output:

```
hello from my first plugin
```

`boot` reconciles every registered plugin to a stable state, then returns a handle per plugin.

## The other plugin shapes

The function form is the most common, but Starriver accepts the same three shapes as Cordis, adapted to Rust:

```rust
use cordis_rslite::{plugin_async, service_sync, Inject, Plugin, Context, EventBus, Service};

// 1. Function plugin (sync).
registry.register(plugin_sync("fn", Inject::none(), |_ctx, _bus| Ok(vec![])));

// 1b. Function plugin (async).
registry.register(plugin_async("fn-async", Inject::none(), |_ctx, _bus| {
    Box::pin(async { Ok(vec![]) })
}));

// 2. Object plugin: a struct implementing the `Plugin` trait.
struct MyPlugin;
impl Plugin for MyPlugin {
    fn name(&self) -> &'static str { "object" }
    fn apply(&self, _ctx: &Context, _bus: &EventBus)
        -> futures::future::BoxFuture<'static, anyhow::Result<Vec<cordis_rslite::EffectHandle>>> {
        Box::pin(async { Ok(vec![]) })
    }
}

// 3. Service plugin: constructs a service and provides it automatically.
struct Greeting;
impl Service for Greeting { const NAME: &'static str = "greeting"; }
registry.register(service_sync::<Greeting, _>("service", Inject::none(), |_ctx| Ok(Greeting)));
```

Use the function form until you need to expose a service; the service form is covered in chapter 3.

## Try breaking it

Make `apply` fail:

```rust
registry.register(plugin_sync("boom", Inject::none(), |_ctx, _bus| {
    anyhow::bail!("apply exploded")
}));
```

`registry.boot(...).await` returns `Err`. A plugin that fails to load is a loud failure, not a skipped entry.

Next: [Lifecycle and effects](02-lifecycle-and-effects.md) — what happens when a plugin unloads.
