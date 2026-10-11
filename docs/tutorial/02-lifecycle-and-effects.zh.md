# 2. 生命周期与 effect

[English](02-lifecycle-and-effects.md) | 中文

Starriver 插件会在 `PluginHandle::shutdown()`、reload 失败或所需服务消失时被 dispose。`apply` 返回的每个 `EffectHandle` 都会被该插件的 fiber 收养，并在 fiber dispose 时按注册顺序的**逆序**撤销。

## Effect

对于内核尚未管理的资源，把它的清理逻辑包装在 `EffectHandle::new` 中并从 `apply` 返回：

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
            // 在这里获取资源；闭包在卸载时释放资源。
            let effect = EffectHandle::new(|| {
                println!("heartbeat cleaned up");
            });
            // 异步清理请改用 EffectHandle::new_async。
            Ok(vec![effect])
        })
    }
}
```

effect 主体在加载期间运行（在 `apply` 内）；disposer 在卸载期间运行。对于生命周期与插件一致的资源，你绝不需要自行调用 disposer。

## 已经属于 effect 的操作

你很少需要亲自构造 `EffectHandle`，因为内置注册 API 已经返回一个：

- `ctx.provide::<S>(...)`：服务会在卸载时移除，且该注册会自动附着到当前 fiber。
- `bus.on::<E, _>(...)`：监听器会在卸载时移除。
- `service_sync` / `service_async`：自动提供的服务会在卸载时移除。

`ctx.provide` 的服务注册会自动附着，因此你不必收集它。监听器注册（`bus.on`）和自定义 `EffectHandle::new` 资源仍需从 `apply` 返回；提前丢弃它们会撤销注册。

## Fiber 状态机

每个已加载插件实例都拥有一个 fiber：

```
Pending → Loading → Active → Disposing → Disposed
                   ↘ Failed
```

- **Pending**：已经声明，但所需服务尚不可用。
- **Loading / Active**：`apply` 正在运行／已经完成。
- **Failed**：`apply` 返回了错误。
- **Disposing / Disposed**：disposer 正在运行／一切均已拆除。

`Disposing` 相当于 Cordis 的 `Unloading`。当依赖缺失时，你会再次遇到 `Pending`。

## 卸载插件

`PluginHandle::shutdown()` 会同步 dispose 其 fiber，按逆序运行所有 disposer，并且是幂等的：

```rust
let handles = registry.boot(&ctx, &bus).await?;
handles[0].shutdown().await;
assert_eq!(handles[0].state(), FiberState::Disposed);
```

或直接运行现成的 example：

```sh
cargo run -p cordis-rslite --example 02_lifecycle
```

> **与 Cordis 的差异：** 服务注册是自动附着的；监听器与自定义 effect 仍以 `Vec<EffectHandle>` 返回。effect 现在是统一的可 await disposer（`EffectHandle::new` 包装同步清理、`EffectHandle::new_async` 接收异步清理），`shutdown`/`dispose` 均为异步。通过 `ctx.plugin` 挂载子插件将在第 6 章介绍。

下一章：[服务](03-services.zh.md)：插件如何共享功能。
