# 2. Lifecycle and effects

English | [中文](02-lifecycle-and-effects.zh.md)

A Starriver plugin is disposed on `PluginHandle::shutdown()`, on a failed reload, or when a required service disappears. Every `EffectHandle` returned from `apply` is adopted by the plugin's fiber and undone when the fiber is disposed — in reverse registration order.

## Effects

For a resource the kernel does not manage, wrap its cleanup in `EffectHandle::new` and return it from `apply`:

```rust
use cordis_rslite::{BoxFuture, Context, EffectHandle, EventBus, Plugin};

struct HeartbeatPlugin;

impl Plugin for HeartbeatPlugin {
    fn name(&self) -> &'static str {
        "heartbeat"
    }

    fn apply(
        &self,
        _ctx: &Context,
        _bus: &EventBus,
    ) -> BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        Box::pin(async {
            println!("heartbeat plugin loading");
            // Acquire the resource here; the closure releases it on unload.
            let effect = EffectHandle::new(|| {
                println!("heartbeat cleaned up");
            });
            // For asynchronous cleanup, use EffectHandle::new_async instead.
            Ok(vec![effect])
        })
    }
}
```

The effect body runs during load (inside `apply`); the disposer runs during unload. You never call the disposer yourself for a plugin-lifetime resource.

## What is already an effect

You rarely construct `EffectHandle` yourself, because the built-in registrations return one already:

- `ctx.provide::<S>(...)` — the service is removed on unload, and the registration is auto-attached to the current fiber.
- `bus.on::<E, _>(...)` — the listener is removed on unload.
- `service_sync` / `service_async` — the provided service is removed on unload.

Service registration via `ctx.provide` is auto-attached, so you do not need to collect it. Listener registration (`bus.on`) and custom `EffectHandle::new` resources must still be returned from `apply`; dropping them early undoes the registration.

## The fiber state machine

Every loaded plugin instance owns a fiber:

```
Pending → Loading → Active → Disposing → Disposed
                   ↘ Failed
```

- **Pending** — declared, but a required service is not available yet.
- **Loading / Active** — `apply` is running / has completed.
- **Failed** — `apply` returned an error.
- **Disposing / Disposed** — disposers are running / everything is torn down.

`Disposing` plays the role of Cordis's `Unloading`. You meet `Pending` again when a dependency is missing.

## Dispose a plugin

`PluginHandle::shutdown()` disposes its fiber synchronously, runs all disposers in reverse order, and is idempotent:

```rust
let handles = registry.boot(&ctx, &bus).await?;
handles[0].shutdown().await;
assert_eq!(handles[0].state(), FiberState::Disposed);
```

Or run the ready-made example:

```sh
cargo run -p cordis-rslite --example 02_lifecycle
```

> **Differences from Cordis:** Service registration is auto-attached; listener and custom effects are returned as a `Vec<EffectHandle>`. Effects are unified awaitable disposers (`EffectHandle::new` wraps sync cleanup, `EffectHandle::new_async` takes async cleanup), and `shutdown`/`dispose` are async. Child-plugin mounting through `ctx.plugin` is covered in chapter 6.

Next: [Services](03-services.md) — how plugins share capabilities.
