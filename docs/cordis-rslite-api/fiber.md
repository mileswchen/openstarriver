# Fiber

A per-plugin lifecycle object that owns cleanup effects and state transitions.

## States

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

Transition flow:

```text
Pending → Loading → Active → Disposing → Disposed
                   ↘ Failed
```

## Public API

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

`dispose` is idempotent and runs adopted cleanups in reverse registration order.
