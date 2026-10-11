# 3. 服务

[English](03-services.md) | 中文

**服务**是一个插件提供、其他插件通过 `ctx` 消费的具名能力。消费方只指定 `"greeter"` 这类能力名，而不导入其提供方，因此配置可以在不改动消费方的情况下更换提供方。

## 提供服务

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

// service 形态插件：构造一个 Greeter 并自动 provide。
let greeter = service_sync::<Greeter, _>("greeter", Inject::none(), |_ctx| Ok(Greeter));
```

`Service::NAME` 是运行时键；`service_sync` 以该名称注册实例，并在卸载时移除它（注册属于 effect）。

## 使用 `inject` 消费服务

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

`inject` 列出该插件所需的服务。Starriver 会让插件保持 `Pending`，直到列出的每项服务都存在，因此在 `apply` 内可以保证 `greeter` 已就绪。注册顺序无关紧要：决定插件何时启动的是依赖关系，而不是顺序。

## 加载后仍会跟踪依赖

`inject` 并非一次性的启动检查。运行期间如果所需服务消失或被替换，依赖插件会随之卸载，并在服务恢复后重新激活。结合 effect（第 2 章），这能防止运行中的消费方持有对不可用服务的引用。

这也是可以替换提供方的原因：提供一个新的 `greeter`，所有注入 `"greeter"` 的插件都会针对新实现重新加载。

## 可选依赖

硬性依赖请使用 `inject`。对于缺失也能运行的能力，请跳过 `inject`，在使用处探测：

```rust
match ctx.require_named::<Greeter>("greeter") {
    Ok(greeter) => println!("{}", greeter.greet("maybe")),
    Err(_) => println!("no greeter available"),
}
```

你也可以用 `ctx.has("greeter")` 只判断是否存在、不做类型下转型。

## 命名

服务名称共用一个扁平命名空间。请为自有服务添加有辨识度的前缀；公开能力建议使用 `seam:` 前缀，并用 `is_public_seam(name)` 判断。内核保留 `kernel:` 前缀给内部键。

下一章：[事件](04-events.zh.md)：无需共享服务即可通信。
