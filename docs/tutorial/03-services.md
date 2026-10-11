# 3. Services

English | [中文](03-services.zh.md)

A **service** is a named capability that one plugin provides and other plugins consume through `ctx`. A consumer names the capability, such as `"greeter"`, rather than importing its provider, so configuration can swap providers without changing the consumer.

## Provide a service

```rust
use cordis_rslite::{service_sync, Inject, Service};

struct Greeter;
impl Service for Greeter {
    const NAME: &'static str = "greeter";
}
impl Greeter {
    fn greet(&self, who: &str) -> String {
        format!("Hello, {}!", who)
    }
}

// The service-form plugin: constructs a `Greeter` and provides it automatically.
let greeter = service_sync::<Greeter, _>("greeter", Inject::none(), |_ctx| Ok(Greeter));
```

`Service::NAME` is the runtime key; `service_sync` registers the instance under that name and removes it on unload (the registration is an effect).

## Consume a service with `inject`

```rust
struct Consumer;
impl Plugin for Consumer {
    fn name(&self) -> &'static str { "consumer" }
    fn inject(&self) -> Inject { Inject::new(["greeter"]) }
    fn apply(&self, ctx: &Context, _bus: &EventBus)
        -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let ctx = ctx.clone();
        Box::pin(async move {
            let greeter = ctx.require_named::<Greeter>("greeter")?;
            println!("{}", greeter.greet("world"));
            Ok(vec![])
        })
    }
}
```

`inject` lists the services the plugin requires. Starriver holds the plugin `Pending` until every listed service exists, so inside `apply`, `greeter` is guaranteed ready. Registration order does not matter: dependencies, not order, decide when plugins start.

## Dependencies are tracked after load

`inject` is not a one-shot boot check. If a required service disappears or is replaced while running, dependents are unloaded and re-activated when the service returns. Combined with effects (chapter 2), this prevents a running consumer from holding a reference to an unavailable service.

This is also why provider replacement works: provide a new `greeter`, and every plugin injecting `"greeter"` reloads against the new implementation.

## Optional dependencies

Use `inject` for hard requirements. For an optional capability, skip `inject` and probe at the use site:

```rust
match ctx.require_named::<Greeter>("greeter") {
    Ok(greeter) => println!("{}", greeter.greet("maybe")),
    Err(_) => println!("no greeter available"),
}
```

You can also check `ctx.has("greeter")` without downcasting.

## Naming

Service names share one flat namespace. Prefix your own services distinctively; use the `seam:` prefix for public capabilities and `is_public_seam(name)` to test for it. The kernel reserves `kernel:` for its own internal keys.

Next: [Events](04-events.md) — communication without a shared service.
