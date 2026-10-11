# 6. 动态注册与作用域

[English](06-dynamic-and-scopes.md) | 中文

第 1 章启动的是固定的插件列表。`cordis-rslite` 也支持在运行时挂载插件、按 context 隔离服务实现，以及按目标 context 过滤事件。

## 动态注册

在启动 registry 之后，`ctx.plugin` 会立即挂载一个新插件。它返回一个 `PluginHandle`，其稳定的 `key()` 可以用来移除或替换该实例：

```rust
use cordis_rslite::{service_sync, Context, EventBus, Inject, PluginRegistry, Service};

struct Greeting(Arc<str>);
impl Service for Greeting { const NAME: &'static str = "greeting"; }

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();

    // 启动一个空 registry，让 `ctx.plugin` 有运行时根可以挂载。
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

- `ctx.plugin(plugin)`：启动插件并返回其句柄。
- `ctx.replace_plugin(old_key, new_plugin)`：移除旧实例并在其位置启动新实例。
- `ctx.remove_plugin(key)`：销毁该实例；如果没有存活实例使用该 key，则返回 `false`。

每次调用 `ctx.plugin` 都会分配新的 `PluginKey`，因此启动同一插件类型两次会得到两个不同的 key。

## 隔离

`ctx.isolate(name)` 会在子 context 中为某个服务名创建一个全新的独立实现槽。对于该名称，子 context 不会继承父 context 的值：

```rust
let child = ctx.isolate("greeting");
let _ = child.provide::<Greeting>(Arc::new(Greeting(Arc::from("isolated"))))?;

assert_eq!(ctx.require::<Greeting>()?.0.as_ref(), "replaced");
assert_eq!(child.require::<Greeting>()?.0.as_ref(), "isolated");
```

要让两个分支加入同一个槽位，请复用显式 label：

```rust
let label = ctx.new_isolation();
let a = ctx.isolate_with("greeting", label);
let b = ctx.isolate_with("greeting", label);
```

`isolate_many(names, label?)` 把多个服务名映射到同一个共享槽位。`new_isolation()` 分配唯一 label，不影响 context。

## 过滤事件

通过 `ctx.on` / `ctx.on_sync` 注册的监听器会记录其所属 context。`*_scoped` 分发会接收一个目标 context；如果该目标带过滤器，过滤器将决定哪些所属监听器运行：

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
bus.emit_sync_scoped(&deny, Arc::new(Ping));  // 仅 global
```

过滤规则：

- 使用 `EventOptions { global: true }` 注册的监听器始终运行。
- 直接注册在 `EventBus`（无 owner）上的监听器始终运行。
- 否则，当目标带有 `ContextFilter` 时，会把它应用到监听器的所属 context；结果为 `false` 则跳过该监听器。
- 当目标没有过滤器时，所有监听器都运行。

`with_filter` 挂载谓词；`ctx.filter()` 返回它。`serial`、`parallel`、`bail` 和 `waterfall` 也有对应的 `*_scoped` 变体。

运行现成的 example：

```sh
cargo run -p cordis-rslite --example 06_dynamic_and_scopes
```
