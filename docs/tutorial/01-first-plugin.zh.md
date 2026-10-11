# 1. 编写第一个插件

[English](01-first-plugin.md) | 中文

Starriver 插件通过一个 **上下文**（`ctx`）和一个 **事件总线**（`bus`）来贡献服务与事件监听器。内核只负责组合插件，不包含任何产品逻辑。

> **与 Cordis 的差异：** Starriver 目前还没有 `cordis.yml` 这类配置文件 loader。应用暂时通过代码里的 `PluginRegistry` 进行组合，配置 loader 计划在后续阶段加入。

## 编写插件

新建一个依赖 `cordis-rslite` 的二进制 crate，编写 `main.rs`：

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

`"hello"` 是显示元数据；闭包就是插件的 `apply` 主体。这里返回空的 `Vec<EffectHandle>`，因为这个插件还没有贡献任何服务或监听器。

## 组合应用

组合在代码中完成：

```rust
let registry = PluginRegistry::new();
registry.register(plugin_sync("hello", Inject::none(), |_ctx, _bus| { Ok(vec![]) }));
registry.register(other_plugin);

let handles = registry.boot(&ctx, &bus).await?;
```

注册顺序并不决定激活顺序。插件按 `inject` 依赖进行 reconcile（见第 3 章），因此消费者可以先于提供者注册。

## 运行

```sh
cargo run
```

或直接运行仓库里现成的 example：

```sh
cargo run -p cordis-rslite --example 01_first_plugin
```

预期输出：

```
hello from my first plugin
```

`boot` 会把所有已注册插件 reconcile 到稳定状态，然后为每个插件返回一个句柄。

## 其他两种插件形态

函数形态最常见，但 Starriver 和 Cordis 一样接受三种形态，只是适配成了 Rust：

```rust
use cordis_rslite::{plugin_async, service_sync, Inject, Plugin, Context, EventBus, Service};

// 1. 函数插件（同步）。
registry.register(plugin_sync("fn", Inject::none(), |_ctx, _bus| Ok(vec![])));

// 1b. 函数插件（异步）。
registry.register(plugin_async("fn-async", Inject::none(), |_ctx, _bus| {
    Box::pin(async { Ok(vec![]) })
}));

// 2. Object 插件：实现 `Plugin` trait 的结构体。
struct MyPlugin;
impl Plugin for MyPlugin {
    fn name(&self) -> &'static str { "object" }
    fn apply(&self, _ctx: &Context, _bus: &EventBus)
        -> futures::future::BoxFuture<'static, anyhow::Result<Vec<cordis_rslite::EffectHandle>>> {
        Box::pin(async { Ok(vec![]) })
    }
}

// 3. Service 插件：构造一个服务并自动 provide。
struct Greeting;
impl Service for Greeting { const NAME: &'static str = "greeting"; }
registry.register(service_sync::<Greeting, _>("service", Inject::none(), |_ctx| Ok(Greeting)));
```

在你需要公开服务之前，请一直使用函数形态；第 3 章会介绍何时使用 service 形态。

## 尝试制造错误

让 `apply` 失败：

```rust
registry.register(plugin_sync("boom", Inject::none(), |_ctx, _bus| {
    anyhow::bail!("apply exploded")
}));
```

`registry.boot(...).await` 会返回 `Err`。插件加载失败会明确报错，不会被静默跳过。

下一章：[生命周期与 effect](02-lifecycle-and-effects.zh.md)：插件卸载时会发生什么。
