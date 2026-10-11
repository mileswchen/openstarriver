# 6. Dynamic registry and scopes

English | [中文](06-dynamic-and-scopes.zh.md)

Chapter 1 booted a fixed list of plugins. `cordis-rslite` also supports mounting plugins at runtime, isolating service implementations per context, and filtering events by a target context.

## Dynamic registry

After booting a registry, `ctx.plugin` mounts a new plugin immediately. It returns a `PluginHandle` whose stable `key()` can later remove or replace that instance:

```rust
use cordis_rslite::{service_sync, Context, EventBus, Inject, PluginRegistry, Service};

struct Greeting(Arc<str>);
impl Service for Greeting { const NAME: &'static str = "greeting"; }

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();

    // Boot an empty registry so `ctx.plugin` has a runtime root to attach to.
    let _ = registry.boot(&ctx, &bus).await?;

    let base = ctx
        .plugin(service_sync::<Greeting, _>(
            "greeting",
            Inject::none(),
            |_ctx| Ok(Greeting(Arc::from("base"))),
        ))
        .await?;

    let replacement = ctx
        .replace_plugin(
            base.key(),
            service_sync::<Greeting, _>(
                "greeting-v2",
                Inject::none(),
                |_ctx| Ok(Greeting(Arc::from("replaced"))),
            ),
        )
        .await?;

    assert!(ctx.remove_plugin(replacement.key()).await);
    Ok(())
}
```

- `ctx.plugin(plugin)` — starts a plugin and returns its handle.
- `ctx.replace_plugin(old_key, new_plugin)` — removes the old instance and starts the new one in its place.
- `ctx.remove_plugin(key)` — disposes the instance and returns `false` if no live instance has that key.

Each `ctx.plugin` call allocates a new `PluginKey`, so starting the same plugin type twice yields two distinct keys.

## Isolation

`ctx.isolate(name)` gives one service name a fresh, independent implementation slot in a child context. The child does not inherit the parent's value for that name:

```rust
let child = ctx.isolate("greeting");
let _ = child.provide::<Greeting>(Arc::new(Greeting(Arc::from("isolated"))))?;

assert_eq!(ctx.require::<Greeting>()?.0.as_ref(), "replaced");
assert_eq!(child.require::<Greeting>()?.0.as_ref(), "isolated");
```

To join two branches onto the same slot, reuse an explicit label:

```rust
let label = ctx.new_isolation();
let a = ctx.isolate_with("greeting", label);
let b = ctx.isolate_with("greeting", label);
```

`isolate_many(names, label?)` maps several service names onto one shared slot. `new_isolation()` allocates a unique label without touching a context.

## Filtered events

Listeners registered through `ctx.on` / `ctx.on_sync` record their owning context. A `*_scoped` dispatch accepts a target context; if that target carries a filter, it decides which owned listeners run:

```rust
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

bus.emit_sync_scoped(&allow, Arc::new(Ping)); // owned + global
bus.emit_sync_scoped(&deny, Arc::new(Ping));  // global only
```

Filtering rule:

- A listener registered with `EventOptions { global: true }` always runs.
- A listener registered directly on `EventBus` (owner-less) always runs.
- Otherwise, when the target has a `ContextFilter`, it is applied to the listener's owning context; `false` skips the listener.
- When the target has no filter, every listener runs.

`with_filter` attaches the predicate; `ctx.filter()` returns it. The same `*_scoped` variants exist for `serial`, `parallel`, `bail`, and `waterfall`.

Run the ready-made example:

```sh
cargo run -p cordis-rslite --example 06_dynamic_and_scopes
```
