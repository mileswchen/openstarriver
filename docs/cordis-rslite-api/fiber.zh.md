# Fiber

每个插件实例的生命周期对象，拥有 cleanup effect 并管理状态转换。

## 状态

```rust
pub enum FiberState {
    Pending,
    Loading,
    Active,
    Failed,
    Disposing,
    Disposed,
}
```

转换流程：

```text
Pending → Loading → Active → Disposing → Disposed
                   ↘ Failed
```

## 公开 API

```rust
impl Fiber {
    pub fn new() -> Arc<Self>;
    pub fn state(&self) -> FiberState;
    pub fn set_loading(&self) -> bool;
    pub fn set_active(&self) -> bool;
    pub fn set_failed(&self) -> bool;
    pub fn set_pending(&self) -> bool;
    pub fn adopt(&self, cleanup: Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>) -> Result<(), &'static str>;
    pub fn adopt_effect(&self, effect: EffectHandle) -> Result<(), &'static str>;
    pub async fn dispose(self: &Arc<Self>);
}
```

`dispose` 是幂等的，并按照注册顺序的逆序执行已收养的 cleanup。
