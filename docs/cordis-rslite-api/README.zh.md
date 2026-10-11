# cordis-rslite API 参考

[English](README.md) | 中文

`cordis-rslite` 是从 Starriver 抽取出来的轻量插件框架，只提供组合原语，不包含任何产品逻辑。

- Crate 名：`cordis-rslite`
- Library 名：`cordis_rslite`

公开模块：

- [Context](context.zh.md) — 作用域服务解析与插件挂载。
- [Events](events.zh.md) — 带类型的同步/异步事件总线。
- [Fiber](fiber.zh.md) — 插件生命周期状态与 cleanup 所有权。
- [Plugin & Registry](plugin.zh.md) — 插件形态与依赖感知启动。
- [Service & Effects](service.zh.md) — service trait、effect handle 与类型擦除值。

框架保持小而稳定的内核，产品 seam 交给 `starriver-kernel`。
