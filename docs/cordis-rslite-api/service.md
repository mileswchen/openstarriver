# Service, Effects, and Values

## Service trait

```rust
pub trait Service: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn is_available(&self) -> bool { true }
}

pub trait TypedService: Service {
    const NAME: &'static str;
    fn is_available(&self) -> bool { true }
}

impl<T: TypedService> Service for T { /* name() => NAME */ }
```

`Service` is object-safe: `Arc<dyn Service>` is a valid value and can be provided with `Context::provide_dyn(name, value)` and resolved with `Context::require_dyn(name)`. Runtime-named services implement `Service` directly.

`TypedService` is the compile-time-named channel that every statically-typed service implements; the zero-argument `provide::<T>()` / `require::<T>()` accessors key off `TypedService::NAME`. A type must not implement both traits.

`is_available` gates dependents at dependency-epoch resolution; when `false`, injecting plugins remain `Pending`. The gate applies to dyn services as well.

## EffectHandle

```rust
pub struct EffectHandle;

impl EffectHandle {
    pub fn new(f: impl FnOnce() + Send + 'static) -> Self;
    pub fn new_async(f: impl FnOnce() -> BoxFuture<'static, ()> + Send + 'static) -> Self;
    pub fn none() -> Self;
    pub fn has_cleanup(&self) -> bool;
    pub fn into_cleanup(self) -> Option<Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>>;
    pub async fn dispose_async(self);
}
```

`EffectHandle` is single-shot and does not run cleanup on `Drop`; dispose it explicitly or hand it to a fiber.

Dropping an `EffectHandle` with pending cleanup emits a warning. Call `forget()` to intentionally suppress the warning.

## Value

```rust
pub struct Value {
    // private fields
}

impl Value {
    pub fn new<T: Any + Send + Sync>(value: T) -> Self;
    pub fn from_arc<T: Any + Send + Sync>(value: Arc<T>) -> Self;
    pub fn downcast<T: Any + Send + Sync>(&self) -> Result<Arc<T>, ValueCastError>;
    pub fn try_downcast<T: Any + Send + Sync>(&self) -> Result<Arc<T>, EventError>;
    pub fn type_name(&self) -> &'static str;
    pub fn is<T: Any>(&self) -> bool;
    pub fn as_any(&self) -> &(dyn Any + Send + Sync);
    pub fn ptr_eq(&self, other: &Value) -> bool;
}
```

`Value` is the type-erased payload used by services, event results, and waterfall accumulators. It keeps the concrete type name for clearer downcast errors. `clone()` performs an `Arc` shallow copy rather than cloning the underlying value, and `ptr_eq()` compares `Arc` identity, not value equality.

## Config

```rust
pub type Config = Value;
```

`Config` is the type-erased configuration passed to plugins. Typed access uses `downcast::<C>()`; the `*_with_config` plugin helpers perform this type check in `validate_config`.

Services that accept per-plugin intercept config can read it from a child context created with `Context::intercept`:

```rust
let ctx = ctx.intercept_typed("greeter", GreeterConfig { prefix: "Hi ".into() });
let configs: Vec<Value> = ctx.intercepts("greeter"); // root-most first
```
