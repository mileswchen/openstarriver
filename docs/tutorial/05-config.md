# 5. Configuration

English | [中文](05-config.zh.md)

Most plugins need boot-time options. `cordis-rslite` represents configuration as a type-erased `Config` (an alias of `Value`) and validates it before a plugin's `apply` runs, so a wrong config is a loud boot failure instead of a runtime surprise.

## Typed config

`Config` is `Value`, so a typed config value is created with `Config::new(...)`:

```rust
use std::sync::Arc;
use cordis_rslite::Config;

#[derive(Clone)]
struct GreeterConfig {
    prefix: Arc<str>,
}

let config = Config::new(GreeterConfig {
    prefix: Arc::from("Hello, "),
});
```

## Config-aware service plugin

`service_sync_with_config` receives the validated `Arc<C>` and constructs the service:

```rust
use std::sync::Arc;
use cordis_rslite::{service_sync_with_config, Inject, Service};

struct Greeter {
    prefix: Arc<str>,
}

impl Service for Greeter {
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

let greeter = service_sync_with_config::<Greeter, GreeterConfig, _>(
    "greeter",
    Inject::none(),
    |_ctx, config| {
        Ok(Greeter {
            prefix: config.prefix.clone(),
        })
    },
);
```

The constructor receives `Arc<GreeterConfig>`. The helper type-checks the `Config` in `validate_config`, so a mismatched config is rejected at boot before `apply` ever runs.

## Config-aware function plugin

`plugin_sync_with_config` passes the validated config to the function body as well:

```rust
use cordis_rslite::{plugin_sync_with_config, Inject, Value};

let counter = plugin_sync_with_config::<u64, _>("counter", Inject::none(), |ctx, _bus, config| {
    ctx.provide_any("count", Value::new(*config))
        .map(|handle| vec![handle])
        .map_err(anyhow::Error::from)
});
```

## Register with config

Pass the config through `PluginRegistry::register_with_config`:

```rust
use cordis_rslite::{Context, EventBus, PluginRegistry};

let ctx = Context::new();
let bus = EventBus::new();
let registry = PluginRegistry::new();

registry.register_with_config(
    greeter,
    Config::new(GreeterConfig {
        prefix: Arc::from("Hello, "),
    }),
);
registry.register_with_config(counter, Config::new(1u64));

let handles = registry.boot(&ctx, &bus).await?;

let greeter = ctx.require::<Greeter>()?;
println!("{}", greeter.greet("world"));
```

## Hot reload

`PluginHandle::update` re-validates the config and reloads the plugin in place:

```rust
let counter = handles.iter().find(|handle| handle.name() == "counter").unwrap();
counter.update(Config::new(5u64)).await?;
```

The previous instance is disposed and the plugin is re-applied with the new config. Services it provides are replaced, which in turn reloads any dependent plugins (see chapter 3).

## Intercept overlays

`ctx.intercept` / `ctx.intercept_typed` produce a child context carrying extra config for a service without mutating the parent. Services read the accumulated chain with `ctx.intercepts(name)`:

```rust
let child = ctx.intercept_typed("greeter", "Configured ");
let configs = child.intercepts("greeter"); // root-most first
```

This is the same mechanism the richer `Inject::require_with(name, config)` uses to attach per-plugin config to a dependency before that plugin's `apply` runs.

Run the ready-made example:

```sh
cargo run -p cordis-rslite --example 05_config
```

Next: [Dynamic registry and scopes](06-dynamic-and-scopes.md) — runtime plugin mounting, isolation, and filtered events.
