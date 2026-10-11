# cordis-rslite Gap Backlog

Tracked gaps found while comparing the current implementation with DSH Cordis and `cordis-rs`. Each item should go through brainstorm → spec → plan → implementation.

## Completed

1. **Waterfall continuation semantics**
   - Typed `on_waterfall` / `waterfall` and dynamic `bus.dynamic()` waterfall are implemented.
   - Legacy fold waterfall API removed.

2. **Listener options and async naming aliases**
   - Added `EventOptions { prepend, global }`.
   - Added `on_with`, `on_sync_with`, `once`, `once_sync`, `once_with`, `once_sync_with`.
   - Added `on_async`, `on_async_with`, `once_async`, `once_async_with` aliases.
   - `global` now participates in `*_scoped` target filtering.

3. **Cordis-compatible `serial` / `bail`**
   - `serial`: ordered, first `Some(value)` wins; `Err` fails closed.
   - `serial_collect`: ordered collect-all with isolated errors.
   - `bail`: ordered bail over asynchronous listeners; `Err` fails closed.
   - `bail_sync`: ordered bail over synchronous listeners.

4. **`internal/get` / `internal/set` integration**
   - Added optional `EventBus` to `Context` and propagated it through `scoped`, `isolate`, and `with_spawner`.
   - Added `Context::with_event_bus`.
   - Added async `require_async` / `require_named_async` routing through `internal/get`.
   - Added async `provide_async` / `provide_named_async` routing through `internal/set`.
   - `PluginRegistry::boot` now injects the bus into the context it passes to plugins.

5. **Unify sync and async listeners across dispatch modes**
   - Merged `on_sync` and `on` into one ordered listener list with a `ListenerKind` discriminator,
     preserving cross-kind registration order and `prepend`.
   - `serial`, `serial_collect`, `serial_strict`, `parallel`, and `bail` dispatch to both
     synchronous and asynchronous listeners.
   - `emit_sync` / `bail_sync` remain synchronous-listener-only paths.
   - Waterfall remains a separate typed `on_waterfall` channel and does not consume regular
     `on_sync` / `on` listeners.

6. **Context filter and event target**
   - Added `ContextFilter`, `Context::with_filter`, and `Context::filter`.
   - `ListenerEntry` / `WaterfallEntry` now record their owning `Context`.
   - `Context` is a first-class event API: `on`, `on_sync`, `once`, `once_sync`,
     `on_with`, `once_with`, `emit`, `serial`, `bail`, `waterfall`, and their
     sync/async variants.
   - `EventBus` gained `emit_scoped`, `serial_scoped`, `parallel_scoped`, `bail_scoped`,
     `waterfall_scoped`, and their `sync`/`collect`/`strict` variants for target
     filtering.
   - Filter rule: `EventOptions::global` or owner-less (`EventBus::on`) listeners
     always run; otherwise the target's `ContextFilter` is applied to the owner.

## Next

1. **Dynamic `String` service keys** — done
   - Added `Context::{provide_any, require_any, provide_any_async, require_any_async}` so
     dynamic/external plugins can register and resolve runtime-named `Value` services without
     implementing `Service`.
   - Typed `Service` accessors still exist for internal plugins and share the same storage core.

2. **Config interception and schema validation** — done
   - Added `Config` type alias (`Value`).
   - Added `Plugin::validate_config` and `Plugin::apply_with_config` with
     backward-compatible defaults; legacy plugins keep implementing `apply`.
   - Added config-aware helpers: `plugin_sync_with_config`,
     `plugin_async_with_config`, `service_sync_with_config`,
     `service_async_with_config`.
   - Added `PluginRegistry::register_with_config`, `PluginHandle::update`,
     and `internal/config` / `internal/update` dynamic waterfall routing.
   - Added `Context::{intercept, intercept_typed, intercepts, intercept_value}`
     for accumulated, parent-immutable intercept overlays.

3. **Full isolation label model** — done
   - Added opaque `Isolation(u64)` with `from_raw` / `as_raw`.
   - Added `Context::{new_isolation, isolate(name), isolate_with(name, label),
     isolate_many(names, label?)}`.
   - Service storage now keys on per-name `Isolation` slots with lazily
     allocated defaults instead of `(scope, name)` with parent-chain fallback.
   - `isolate(name)` now creates a fresh empty slot (no inheritance), matching
     Cordis. `isolate_with(name, label)` and `isolate_many` join slots.
   - Resolved divergence: `provide`, `provide_named`, and `provide_any` now
     return `Result<EffectHandle, ContextError>` and reject a second provide
     into the same scope with `ContextError::Duplicate`. Plugin reloads clear
     the previous effects before re-applying so hot reloads can re-provide
     without colliding with the old implementation.

4. **Plugin form and runtime registry completeness** — partially done
   - Done: dynamic add / remove / replace via `Context::{plugin,
     plugin_with_config, remove_plugin, replace_plugin,
     replace_plugin_with_config}` and stable `PluginKey`.
   - Done: `remove_plugin` now physically deletes the entry from the runtime
     `HashMap<PluginKey, PluginEntry>` instead of leaving a tombstone, matching
     Cordis's runtime map deletion.
   - Done: richer `Inject` (`Dependency { name, config }` plus `require`,
     `require_with`, `require_with_value`); per-dependency intercept config is
     applied to the plugin's scoped context before `apply`.
   - Deferred: class-like service init (`Service::init`); keep using
     `service_sync` / `service_async` constructors for initialization for now.

5. **Effect lifecycle ergonomics**
   - Added `#[must_use]`, Drop warning, and `EffectHandle::forget`.

6. **Value helpers and event declaration ergonomics** — partially done
   - Done: `Value::{as_any, ptr_eq}` and the `cordis_event!` declaration macro.
   - Done: `Value` downcast ergonomics — `EventError::Cast` and
     `ContextError::Cast` now carry `ValueCastError` via `#[from]`, and
     `try_downcast` reports the expected/actual type names instead of a generic
     listener string. Dynamic event `arg` dispatch now goes through
     `try_downcast`.
   - Done: manual `event_catalog!` macro generating a function that returns
     `Vec<(&'static str, &'static str)>`.
   - Deferred: automatic catalog discovery via `build.rs` or proc-macro
     registration; wait for `starriver-kernel` bindgen/proc-macro strategy.

7. **Documentation and tutorial completeness** — done
   - Added tutorial chapters 5 (config / schema validation / intercept / hot
     reload) and 6 (dynamic registry / isolation / filtered events), in both
     English and Chinese.
   - Added `examples/06_dynamic_and_scopes.rs`.
   - Updated the API reference to include `EventError::Cast` and
     `ContextError::{Duplicate, Cast}`, and refreshed the tutorial prose that
     still used the pre-wrapper `Arc::new(()) as Value` pattern.

8. **Dead API cleanup** — done
   - Removed `DispatchPolicy` and `EventBus::set_policy`, which had become
     unreachable configuration: no dispatch path read the stored policy after
     `serial` / `bail` were made fail-closed and `emit` / `parallel` kept their
     isolated or strict variants. Failure behavior is now fixed per dispatch
     method.

9. **Object-safe `Service` (dyn support)** — done
   - `Service` no longer carries `const NAME` (E0038 root cause). It now
     exposes `fn name(&self) -> &'static str` and stays object-safe:
     `Arc<dyn Service>` is a first-class value.
   - Added `TypedService: Service { const NAME }` with a blanket impl into
     `Service`; all statically-typed services moved to `TypedService` and the
     zero-argument `provide::<T>()` / `require::<T>()` accessors keep their
     compile-time name resolution.
   - Added `Context::provide_dyn(name, Arc<dyn Service>)` /
     `Context::require_dyn(name)` with the `is_available` gate preserved
     (values ride through a private concrete wrapper).
   - Design note: docs/superpowers/specs/2026-09-08-service-trait-dyn-design.md
