# Service、Effect 与 Value

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

`Service` 是对象安全的:`Arc<dyn Service>` 是合法服务值,可通过 `Context::provide_dyn(name, value)` 提供、`Context::require_dyn(name)` 解析。运行时命名的服务直接实现 `Service`。

`TypedService` 是编译期命名通道,所有静态类型服务都实现它;零参访问器 `provide::<T>()` / `require::<T>()` 以 `TypedService::NAME` 为键。同一类型不得同时实现两个 trait。

`is_available` 在 dependency epoch 解析时门控依赖方；返回 `false` 时,注入该服务的插件会保持 `Pending`。该门控对 dyn 服务同样生效。

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

`EffectHandle` 是单次使用的，不会在 `Drop` 时运行 cleanup；请显式 dispose，或交给 fiber 收养。

丢弃仍持有 cleanup 的 `EffectHandle` 会输出警告；有意丢弃时请调用 `forget()`。

## Value

```rust
pub struct Value {
    // 私有字段
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

`Value` 是服务、事件结果和 waterfall 累加器使用的类型擦除负载。它保留具体类型名，以便 downcast 失败时给出更清晰的错误。`clone()` 执行的是 `Arc` 浅拷贝，不会克隆底层数据；`ptr_eq()` 比较的是 `Arc` 身份，而不是值相等。

## Config

```rust
pub type Config = Value;
```

`Config` 是传递给插件的类型擦除配置。类型化访问使用 `downcast::<C>()`；`*_with_config` 系列 helper 会在 `validate_config` 中执行这一类型检查。

接受逐插件拦截配置的服务，可以从 `Context::intercept` 创建的子 context 中读取：

```rust
let ctx = ctx.intercept_typed("greeter", GreeterConfig { prefix: "Hi ".into() });
let configs: Vec<Value> = ctx.intercepts("greeter"); // 根优先
```
