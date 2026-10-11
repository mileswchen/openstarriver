# 5. 配置

[English](05-config.md) | 中文

大多数插件都需要启动时的选项。`cordis-rslite` 把配置表示为类型擦除的 `Config`（即 `Value` 的别名），并在插件 `apply` 运行之前校验它，因此配置错误会成为响亮的启动失败，而不是运行时的意外。

## 带类型的配置

`Config` 就是 `Value`，因此用 `Config::new(...)` 创建带类型的配置值：

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

## 带配置的 service 插件

`service_sync_with_config` 接收校验后的 `Arc<C>` 并构造服务：

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

构造器接收 `Arc<GreeterConfig>`。该 helper 会在 `validate_config` 中做类型检查，因此类型不匹配的配置会在 `apply` 运行之前就被拒绝。

## 带配置的函数插件

`plugin_sync_with_config` 同样把校验后的配置传给函数体：

```rust
use cordis_rslite::{plugin_sync_with_config, Inject, Value};

let counter = plugin_sync_with_config::<u64, _>("counter", Inject::none(), |ctx, _bus, config| {
    ctx.provide_any("count", Value::new(*config))
        .map(|handle| vec![handle])
        .map_err(anyhow::Error::from)
});
```

## 带配置注册

通过 `PluginRegistry::register_with_config` 传入配置：

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

## 热重载

`PluginHandle::update` 会重新校验配置并在原地重载插件：

```rust
let counter = handles.iter().find(|handle| handle.name() == "counter").unwrap();
counter.update(Config::new(5u64)).await?;
```

旧实例会被 dispose，插件使用新配置重新 `apply`。它提供的服务会被替换，从而进一步重载依赖这些服务的插件（见第 3 章）。

## 拦截配置叠加

`ctx.intercept` / `ctx.intercept_typed` 会产生一个子 context，为某个服务携带额外配置，且不会修改父 context。服务通过 `ctx.intercepts(name)` 读取累积的拦截链：

```rust
let child = ctx.intercept_typed("greeter", "Configured ");
let configs = child.intercepts("greeter"); // 根优先
```

这也是更丰富的 `Inject::require_with(name, config)` 所用的机制：在插件 `apply` 运行之前，把逐插件配置挂到依赖上。

运行现成的 example：

```sh
cargo run -p cordis-rslite --example 05_config
```

下一章：[动态注册与作用域](06-dynamic-and-scopes.zh.md)：运行时插件挂载、隔离与过滤事件。
