# cordis-rslite API Reference

English | [中文](README.zh.md)

`cordis-rslite` is the lightweight plugin framework extracted from Starriver. It provides composition primitives only: no product logic.

- Crate name: `cordis-rslite`
- Library name: `cordis_rslite`

Public modules:

- [Context](context.md) — scoped service resolution and plugin mounting.
- [Events](events.md) — typed event bus with synchronous and asynchronous listeners.
- [Fiber](fiber.md) — plugin lifecycle state and cleanup ownership.
- [Plugin & Registry](plugin.md) — plugin forms and dependency-aware boot.
- [Service & Effects](service.md) — service trait, effect handles, and type-erased values.

The framework keeps a small stable core and leaves product seams to `starriver-kernel`.
