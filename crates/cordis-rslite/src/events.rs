use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::{join_all, BoxFuture, FutureExt};
use tokio::sync::Semaphore;

use crate::context::Context;
use crate::effect::EffectHandle;
use crate::value::Value;

pub trait Event: Send + Sync + 'static {
    const NAME: &'static str;
}

pub trait WaterfallEvent: Event {
    type Output: Send + Sync + 'static + Clone;
}

pub type Next<O> = Arc<dyn Fn() -> BoxFuture<'static, Result<O, EventError>> + Send + Sync>;

#[derive(Clone)]
pub struct DynamicEvent {
    name: Arc<str>,
    args: Arc<[Value]>,
}

impl DynamicEvent {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn args(&self) -> &[Value] {
        &self.args
    }

    pub fn arg<T>(&self, index: usize) -> Result<Option<Arc<T>>, EventError>
    where
        T: Send + Sync + 'static,
    {
        self.args
            .get(index)
            .map(|value| value.try_downcast::<T>())
            .transpose()
    }
}

pub type DynamicNext =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Option<Value>, EventError>> + Send + Sync>;

type DynamicWaterfallListener = Arc<
    dyn Fn(DynamicEvent, DynamicNext) -> BoxFuture<'static, Result<Option<Value>, EventError>>
        + Send
        + Sync,
>;

#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("payload for event `{0}` has the wrong type")]
    TypeMismatch(&'static str),
    #[error("value cast failed: {0}")]
    Cast(#[from] crate::value::ValueCastError),
    #[error("listener failed: {0}")]
    Listener(String),
    #[error("parallel listeners failed: {0:?}")]
    Aggregate(Vec<String>),
    #[error("waterfall listener timed out")]
    Timeout,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventOptions {
    pub prepend: bool,
    pub global: bool,
}

type Listener =
    Arc<dyn Fn(Value) -> BoxFuture<'static, Result<Option<Value>, EventError>> + Send + Sync>;
type SyncListener = Arc<dyn Fn(Value) -> Result<Option<Value>, EventError> + Send + Sync>;
type ErasedWaterfallListener =
    Arc<dyn Fn(Value, Next<Value>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync>;

#[derive(Clone)]
enum ListenerKind {
    Sync(SyncListener),
    Async(Listener),
}

#[derive(Default)]
struct Entry {
    listeners: Vec<ListenerEntry>,
    waterfall: Vec<WaterfallEntry>,
}

struct ListenerEntry {
    id: u64,
    options: EventOptions,
    kind: ListenerKind,
    owner: Option<Context>,
}

struct WaterfallEntry {
    id: u64,
    owner: Option<Context>,
    listener: ErasedWaterfallListener,
}

struct Inner {
    events: Mutex<HashMap<&'static str, Entry>>,
    dynamic_waterfall: Mutex<HashMap<String, Vec<(u64, DynamicWaterfallListener)>>>,
    next_id: AtomicU64,
    semaphore: Arc<Semaphore>,
    dropped_emits: AtomicU64,
    emit_limit: usize,
    listener_calls: AtomicU64,
    listener_total_ns: AtomicU64,
    dispatch_calls: AtomicU64,
    dispatch_total_ns: AtomicU64,
    waterfall_timeout: Mutex<Option<Duration>>,
}

#[derive(Clone)]
pub struct EventBus {
    inner: Arc<Inner>,
}

#[derive(Clone)]
pub struct DynamicEvents {
    bus: EventBus,
}

fn remove_listener(inner: &Arc<Inner>, name: &'static str, id: u64) {
    if let Ok(mut events) = inner.events.lock() {
        let mut remove = false;
        if let Some(entry) = events.get_mut(name) {
            entry.listeners.retain(|entry| entry.id != id);
            remove = entry.listeners.is_empty() && entry.waterfall.is_empty();
        }
        if remove {
            events.remove(name);
        }
    }
}

fn listener_accepted(entry: &ListenerEntry, target: Option<&Context>) -> bool {
    if entry.options.global {
        return true;
    }
    let Some(filter) = target.and_then(|target| target.filter()) else {
        return true;
    };
    match &entry.owner {
        // Listeners registered directly on the bus (no owner) are global and
        // therefore unaffected by target filtering.
        None => true,
        Some(owner) => filter(owner),
    }
}

fn waterfall_accepted(entry: &WaterfallEntry, target: Option<&Context>) -> bool {
    let Some(filter) = target.and_then(|target| target.filter()) else {
        return true;
    };
    match &entry.owner {
        None => true,
        Some(owner) => filter(owner),
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self::with_emit_limit(1024)
    }

    pub fn with_emit_limit(limit: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                events: Mutex::new(HashMap::new()),
                dynamic_waterfall: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(0),
                semaphore: Arc::new(Semaphore::new(limit)),
                dropped_emits: AtomicU64::new(0),
                emit_limit: limit,
                listener_calls: AtomicU64::new(0),
                listener_total_ns: AtomicU64::new(0),
                dispatch_calls: AtomicU64::new(0),
                dispatch_total_ns: AtomicU64::new(0),
                waterfall_timeout: Mutex::new(None),
            }),
        }
    }

    pub fn dynamic(&self) -> DynamicEvents {
        DynamicEvents { bus: self.clone() }
    }

    pub fn on<E, F>(&self, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.on_with(listener, EventOptions::default())
    }

    pub fn on_async<E, F>(&self, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.on(listener)
    }

    pub fn on_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.register_listener(None, listener, options, false)
    }

    pub fn on_async_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.on_with(listener, options)
    }

    pub fn once<E, F>(&self, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.once_with(listener, EventOptions::default())
    }

    pub fn once_async<E, F>(&self, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.once(listener)
    }

    pub fn once_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.register_listener(None, listener, options, true)
    }

    pub fn once_async_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.once_with(listener, options)
    }

    pub(crate) fn on_owned<E, F>(&self, owner: &Context, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.register_listener(
            Some(owner.clone()),
            listener,
            EventOptions::default(),
            false,
        )
    }

    pub(crate) fn on_with_owned<E, F>(
        &self,
        owner: &Context,
        listener: F,
        options: EventOptions,
    ) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.register_listener(Some(owner.clone()), listener, options, false)
    }

    pub(crate) fn once_owned<E, F>(&self, owner: &Context, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.register_listener(Some(owner.clone()), listener, EventOptions::default(), true)
    }

    pub(crate) fn once_with_owned<E, F>(
        &self,
        owner: &Context,
        listener: F,
        options: EventOptions,
    ) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static,
    {
        self.register_listener(Some(owner.clone()), listener, options, true)
    }

    pub(crate) fn on_sync_owned<E, F>(&self, owner: &Context, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static,
    {
        self.register_sync_listener(
            Some(owner.clone()),
            listener,
            EventOptions::default(),
            false,
        )
    }

    pub(crate) fn on_sync_with_owned<E, F>(
        &self,
        owner: &Context,
        listener: F,
        options: EventOptions,
    ) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static,
    {
        self.register_sync_listener(Some(owner.clone()), listener, options, false)
    }

    pub(crate) fn once_sync_owned<E, F>(&self, owner: &Context, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static,
    {
        self.register_sync_listener(Some(owner.clone()), listener, EventOptions::default(), true)
    }

    pub(crate) fn once_sync_with_owned<E, F>(
        &self,
        owner: &Context,
        listener: F,
        options: EventOptions,
    ) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static,
    {
        self.register_sync_listener(Some(owner.clone()), listener, options, true)
    }

    fn register_listener<E, F>(
        &self,
        owner: Option<Context>,
        listener: F,
        options: EventOptions,
        once: bool,
    ) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> BoxFuture<'static, Result<Value, EventError>> + Send + Sync + 'static,
    {
        let name = E::NAME;
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let inner = Arc::clone(&self.inner);
        let effect_inner = Arc::clone(&inner);
        let fired = once.then(|| Arc::new(AtomicBool::new(false)));

        let wrapped: Listener = Arc::new(move |value: Value| {
            let event = match value.downcast::<E>() {
                Ok(event) => event,
                Err(_) => return Box::pin(async { Err(EventError::TypeMismatch(name)) }),
            };
            if let Some(fired) = fired.as_ref() {
                if fired.swap(true, Ordering::SeqCst) {
                    return Box::pin(async { Ok(None) });
                }
                remove_listener(&inner, name, id);
            }
            let future = listener(event);
            Box::pin(async move {
                let value = future.await?;
                Ok(Some(value))
            })
        });

        {
            let mut events = self.inner.events.lock().expect("event map poisoned");
            let entry = events.entry(name).or_default();
            let entry_item = ListenerEntry {
                id,
                options,
                kind: ListenerKind::Async(wrapped),
                owner,
            };
            if options.prepend {
                entry.listeners.insert(0, entry_item);
            } else {
                entry.listeners.push(entry_item);
            }
        }

        EffectHandle::new(move || remove_listener(&effect_inner, name, id))
    }

    pub fn on_sync<E, F>(&self, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static,
    {
        self.on_sync_with(listener, EventOptions::default())
    }

    pub fn on_sync_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static,
    {
        self.register_sync_listener(None, listener, options, false)
    }

    pub fn once_sync<E, F>(&self, listener: F) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static,
    {
        self.once_sync_with(listener, EventOptions::default())
    }

    pub fn once_sync_with<E, F>(&self, listener: F, options: EventOptions) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static,
    {
        self.register_sync_listener(None, listener, options, true)
    }

    fn register_sync_listener<E, F>(
        &self,
        owner: Option<Context>,
        listener: F,
        options: EventOptions,
        once: bool,
    ) -> EffectHandle
    where
        E: Event,
        F: Fn(Arc<E>) -> Result<Value, EventError> + Send + Sync + 'static,
    {
        let name = E::NAME;
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let inner = Arc::clone(&self.inner);
        let effect_inner = Arc::clone(&inner);
        let fired = once.then(|| Arc::new(AtomicBool::new(false)));

        let wrapped: SyncListener = Arc::new(move |value: Value| {
            let event = value
                .downcast::<E>()
                .map_err(|_| EventError::TypeMismatch(name))?;
            if let Some(fired) = fired.as_ref() {
                if fired.swap(true, Ordering::SeqCst) {
                    return Ok(None);
                }
                remove_listener(&inner, name, id);
            }
            Ok(Some(listener(event)?))
        });

        {
            let mut events = self.inner.events.lock().expect("event map poisoned");
            let entry = events.entry(name).or_default();
            let entry_item = ListenerEntry {
                id,
                options,
                kind: ListenerKind::Sync(wrapped),
                owner,
            };
            if options.prepend {
                entry.listeners.insert(0, entry_item);
            } else {
                entry.listeners.push(entry_item);
            }
        }

        EffectHandle::new(move || remove_listener(&effect_inner, name, id))
    }

    pub fn on_waterfall<E, F>(&self, listener: F) -> EffectHandle
    where
        E: WaterfallEvent,
        F: Fn(Arc<E>, Next<E::Output>) -> BoxFuture<'static, Result<E::Output, EventError>>
            + Send
            + Sync
            + 'static,
    {
        self.register_waterfall(None, listener)
    }

    pub(crate) fn on_waterfall_owned<E, F>(&self, owner: &Context, listener: F) -> EffectHandle
    where
        E: WaterfallEvent,
        F: Fn(Arc<E>, Next<E::Output>) -> BoxFuture<'static, Result<E::Output, EventError>>
            + Send
            + Sync
            + 'static,
    {
        self.register_waterfall(Some(owner.clone()), listener)
    }

    fn register_waterfall<E, F>(&self, owner: Option<Context>, listener: F) -> EffectHandle
    where
        E: WaterfallEvent,
        F: Fn(Arc<E>, Next<E::Output>) -> BoxFuture<'static, Result<E::Output, EventError>>
            + Send
            + Sync
            + 'static,
    {
        let name = E::NAME;
        let wrapped: ErasedWaterfallListener = Arc::new(move |value, next| {
            let event = match value.downcast::<E>() {
                Ok(event) => event,
                Err(_) => return Box::pin(async { Err(EventError::TypeMismatch(name)) }),
            };
            let typed_next: Next<E::Output> = Arc::new(move || {
                let next = Arc::clone(&next);
                Box::pin(async move {
                    let value = next().await?;
                    let output = value
                        .downcast::<E::Output>()
                        .map_err(|_| EventError::TypeMismatch(name))?;
                    Ok(output.as_ref().clone())
                })
            });
            let future = listener(event, typed_next);
            Box::pin(async move {
                let output = future.await?;
                Ok(Value::new(output))
            })
        });

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut events = self.inner.events.lock().expect("event map poisoned");
            let entry = events.entry(name).or_default();
            entry.waterfall.push(WaterfallEntry {
                id,
                owner,
                listener: wrapped,
            });
        }
        let inner = Arc::clone(&self.inner);
        EffectHandle::new(move || {
            if let Ok(mut events) = inner.events.lock() {
                let mut remove = false;
                if let Some(entry) = events.get_mut(name) {
                    entry.waterfall.retain(|e| e.id != id);
                    remove = entry.listeners.is_empty() && entry.waterfall.is_empty();
                }
                if remove {
                    events.remove(name);
                }
            }
        })
    }

    pub fn set_waterfall_timeout(&self, timeout: Duration) {
        *self
            .inner
            .waterfall_timeout
            .lock()
            .expect("waterfall timeout poisoned") = Some(timeout);
    }

    pub fn dropped_emits(&self) -> u64 {
        self.inner.dropped_emits.load(Ordering::SeqCst)
    }

    pub fn emit_in_flight(&self) -> usize {
        self.inner
            .emit_limit
            .saturating_sub(self.inner.semaphore.available_permits())
    }

    pub fn emit_limit(&self) -> usize {
        self.inner.emit_limit
    }

    pub fn listener_count(&self, name: &'static str) -> usize {
        self.inner
            .events
            .lock()
            .expect("event map poisoned")
            .get(name)
            .map(|entry| entry.listeners.len() + entry.waterfall.len())
            .unwrap_or(0)
    }

    pub fn listener_calls(&self) -> u64 {
        self.inner.listener_calls.load(Ordering::SeqCst)
    }

    pub fn listener_total_ns(&self) -> u64 {
        self.inner.listener_total_ns.load(Ordering::SeqCst)
    }

    pub fn listener_avg_ns(&self) -> u64 {
        self.listener_total_ns()
            .checked_div(self.listener_calls())
            .unwrap_or(0)
    }

    pub fn dispatch_calls(&self) -> u64 {
        self.inner.dispatch_calls.load(Ordering::SeqCst)
    }

    pub fn dispatch_total_ns(&self) -> u64 {
        self.inner.dispatch_total_ns.load(Ordering::SeqCst)
    }

    pub fn dispatch_avg_ns(&self) -> u64 {
        self.dispatch_total_ns()
            .checked_div(self.dispatch_calls())
            .unwrap_or(0)
    }

    fn record_listener(&self, start: Instant) {
        self.inner.listener_calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .listener_total_ns
            .fetch_add(start.elapsed().as_nanos() as u64, Ordering::SeqCst);
    }

    fn record_dispatch(&self, start: Instant) {
        self.inner.dispatch_calls.fetch_add(1, Ordering::SeqCst);
        self.inner
            .dispatch_total_ns
            .fetch_add(start.elapsed().as_nanos() as u64, Ordering::SeqCst);
    }

    fn snapshot<E: Event>(&self, target: Option<&Context>) -> Vec<Listener> {
        self.inner
            .events
            .lock()
            .expect("event map poisoned")
            .get(E::NAME)
            .map(|e| {
                e.listeners
                    .iter()
                    .filter(|entry| listener_accepted(entry, target))
                    .filter_map(|entry| match &entry.kind {
                        ListenerKind::Async(listener) => Some(Arc::clone(listener)),
                        ListenerKind::Sync(_) => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn snapshot_sync<E: Event>(&self, target: Option<&Context>) -> Vec<SyncListener> {
        self.inner
            .events
            .lock()
            .expect("event map poisoned")
            .get(E::NAME)
            .map(|e| {
                e.listeners
                    .iter()
                    .filter(|entry| listener_accepted(entry, target))
                    .filter_map(|entry| match &entry.kind {
                        ListenerKind::Sync(listener) => Some(Arc::clone(listener)),
                        ListenerKind::Async(_) => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn snapshot_ordered<E: Event>(&self, target: Option<&Context>) -> Vec<ListenerKind> {
        self.inner
            .events
            .lock()
            .expect("event map poisoned")
            .get(E::NAME)
            .map(|e| {
                e.listeners
                    .iter()
                    .filter(|entry| listener_accepted(entry, target))
                    .map(|entry| entry.kind.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn snapshot_waterfall<E: Event>(
        &self,
        target: Option<&Context>,
    ) -> Vec<ErasedWaterfallListener> {
        self.inner
            .events
            .lock()
            .expect("event map poisoned")
            .get(E::NAME)
            .map(|e| {
                e.waterfall
                    .iter()
                    .filter(|entry| waterfall_accepted(entry, target))
                    .map(|entry| Arc::clone(&entry.listener))
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn run_listener(
        &self,
        listener: Listener,
        value: Value,
    ) -> Result<Option<Value>, EventError> {
        let start = Instant::now();
        let result = match std::panic::AssertUnwindSafe(listener(value))
            .catch_unwind()
            .await
        {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(EventError::Listener("listener panicked".into())),
        };
        self.record_listener(start);
        result
    }

    async fn run_listener_kind(
        &self,
        kind: ListenerKind,
        value: Value,
    ) -> Result<Option<Value>, EventError> {
        match kind {
            ListenerKind::Async(listener) => self.run_listener(listener, value).await,
            ListenerKind::Sync(listener) => {
                let start = Instant::now();
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(value)));
                self.record_listener(start);
                match result {
                    Ok(Ok(v)) => Ok(v),
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(EventError::Listener("listener panicked".into())),
                }
            }
        }
    }

    pub fn emit<E: Event>(&self, event: Arc<E>) {
        self.emit_inner(None, event)
    }

    /// Emit an event targeting a context so listeners are filtered by that
    /// context's [`Context::filter`].
    pub fn emit_scoped<E: Event>(&self, target: &Context, event: Arc<E>) {
        self.emit_inner(Some(target), event)
    }

    fn emit_inner<E: Event>(&self, target: Option<&Context>, event: Arc<E>) {
        // Synchronous listeners run immediately in registration order; their
        // results and errors are intentionally ignored by `emit` so a slow or
        // failing observer cannot block later listeners.
        let _ = self.emit_sync_inner(target, event.clone());

        for l in self.snapshot::<E>(target) {
            let value: Value = Value::from_arc(event.clone());
            let bus = self.clone();
            let sem = Arc::clone(&self.inner.semaphore);
            if let Ok(permit) = sem.try_acquire_owned() {
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = bus.run_listener(l, value).await;
                });
            } else {
                self.inner.dropped_emits.fetch_add(1, Ordering::SeqCst);
                tracing::debug!("emit listener dropped: pool exhausted");
            }
        }
    }

    /// Dispatch synchronously to every sync listener registered with `on_sync`.
    ///
    /// Listeners run in registration order. A panic is contained and returned
    /// as [`EventError::Listener`]; all sync listeners still run.
    pub fn emit_sync<E: Event>(&self, event: Arc<E>) -> Vec<Result<Value, EventError>> {
        self.emit_sync_inner(None, event)
    }

    pub fn emit_sync_scoped<E: Event>(
        &self,
        target: &Context,
        event: Arc<E>,
    ) -> Vec<Result<Value, EventError>> {
        self.emit_sync_inner(Some(target), event)
    }

    fn emit_sync_inner<E: Event>(
        &self,
        target: Option<&Context>,
        event: Arc<E>,
    ) -> Vec<Result<Value, EventError>> {
        let start = Instant::now();
        let listeners = self.snapshot_sync::<E>(target);
        let mut results = Vec::with_capacity(listeners.len());

        for listener in listeners {
            let listener_start = Instant::now();
            let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                listener(Value::from_arc(event.clone()))
            })) {
                Ok(Ok(Some(value))) => Ok(value),
                // The once-guard's "already fired" answer still counts as a
                // call — record metrics BEFORE continuing (it skipped them).
                Ok(Ok(None)) => {
                    self.record_listener(listener_start);
                    continue;
                }
                Ok(Err(error)) => Err(error),
                Err(_) => Err(EventError::Listener("listener panicked".into())),
            };
            self.record_listener(listener_start);
            results.push(result);
        }

        self.record_dispatch(start);
        results
    }

    /// Ordered dispatch that returns the first meaningful value.
    ///
    /// Listeners run in registration order. The first listener returning
    /// `Ok(Some(value))` wins and stops the chain. `Ok(None)` means "no
    /// contribution" and continues. `Err` is fail-closed and aborts the chain.
    pub async fn serial<E: Event>(&self, event: Arc<E>) -> Result<Option<Value>, EventError> {
        self.serial_inner(None, event).await
    }

    pub async fn serial_scoped<E: Event>(
        &self,
        target: &Context,
        event: Arc<E>,
    ) -> Result<Option<Value>, EventError> {
        self.serial_inner(Some(target), event).await
    }

    async fn serial_inner<E: Event>(
        &self,
        target: Option<&Context>,
        event: Arc<E>,
    ) -> Result<Option<Value>, EventError> {
        let start = Instant::now();
        for kind in self.snapshot_ordered::<E>(target) {
            let result = self
                .run_listener_kind(kind, Value::from_arc(event.clone()))
                .await;
            match result {
                Ok(Some(value)) => {
                    self.record_dispatch(start);
                    return Ok(Some(value));
                }
                Ok(None) => continue,
                Err(error) => {
                    self.record_dispatch(start);
                    return Err(error);
                }
            }
        }
        self.record_dispatch(start);
        Ok(None)
    }

    /// Ordered dispatch that collects every listener result.
    ///
    /// Errors are isolated by default: a failing listener does not prevent
    /// later listeners from running. Use [`EventBus::serial`] for the
    /// Cordis-compatible first-value-wins semantics.
    pub async fn serial_collect<E: Event>(&self, event: Arc<E>) -> Vec<Result<Value, EventError>> {
        self.serial_collect_inner(None, event).await
    }

    pub async fn serial_collect_scoped<E: Event>(
        &self,
        target: &Context,
        event: Arc<E>,
    ) -> Vec<Result<Value, EventError>> {
        self.serial_collect_inner(Some(target), event).await
    }

    async fn serial_collect_inner<E: Event>(
        &self,
        target: Option<&Context>,
        event: Arc<E>,
    ) -> Vec<Result<Value, EventError>> {
        let start = Instant::now();
        let mut out = Vec::new();
        for kind in self.snapshot_ordered::<E>(target) {
            let result = self
                .run_listener_kind(kind, Value::from_arc(event.clone()))
                .await;
            match result {
                Ok(Some(value)) => out.push(Ok(value)),
                Ok(None) => {}
                Err(error) => out.push(Err(error)),
            }
        }
        self.record_dispatch(start);
        out
    }

    /// Fail-closed serial dispatch: returns an error on the first failing listener.
    pub async fn serial_strict<E: Event>(&self, event: Arc<E>) -> Result<Vec<Value>, EventError> {
        self.serial_strict_inner(None, event).await
    }

    pub async fn serial_strict_scoped<E: Event>(
        &self,
        target: &Context,
        event: Arc<E>,
    ) -> Result<Vec<Value>, EventError> {
        self.serial_strict_inner(Some(target), event).await
    }

    async fn serial_strict_inner<E: Event>(
        &self,
        target: Option<&Context>,
        event: Arc<E>,
    ) -> Result<Vec<Value>, EventError> {
        let start = Instant::now();
        let mut out = Vec::new();
        for kind in self.snapshot_ordered::<E>(target) {
            if let Some(value) = self
                .run_listener_kind(kind, Value::from_arc(event.clone()))
                .await?
            {
                out.push(value);
            }
        }
        self.record_dispatch(start);
        Ok(out)
    }

    pub async fn parallel<E: Event>(&self, event: Arc<E>) -> Result<(), EventError> {
        self.parallel_inner(None, event).await
    }

    pub async fn parallel_scoped<E: Event>(
        &self,
        target: &Context,
        event: Arc<E>,
    ) -> Result<(), EventError> {
        self.parallel_inner(Some(target), event).await
    }

    async fn parallel_inner<E: Event>(
        &self,
        target: Option<&Context>,
        event: Arc<E>,
    ) -> Result<(), EventError> {
        let start = Instant::now();
        let kinds = self.snapshot_ordered::<E>(target);
        let this = self.clone();
        let results = join_all(kinds.into_iter().map(|kind| {
            let e = Value::from_arc(event.clone());
            let bus = this.clone();
            async move { bus.run_listener_kind(kind, e).await }
        }))
        .await;
        let errors = results
            .into_iter()
            .filter_map(|result| result.err())
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        self.record_dispatch(start);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(EventError::Aggregate(errors))
        }
    }

    /// Ordered bail dispatch over both synchronous and asynchronous listeners.
    ///
    /// Returns the first `Ok(Some(value))` and stops. `Err` is fail-closed.
    pub async fn bail<E: Event>(&self, event: Arc<E>) -> Result<Option<Value>, EventError> {
        self.bail_inner(None, event).await
    }

    pub async fn bail_scoped<E: Event>(
        &self,
        target: &Context,
        event: Arc<E>,
    ) -> Result<Option<Value>, EventError> {
        self.bail_inner(Some(target), event).await
    }

    async fn bail_inner<E: Event>(
        &self,
        target: Option<&Context>,
        event: Arc<E>,
    ) -> Result<Option<Value>, EventError> {
        let start = Instant::now();
        for kind in self.snapshot_ordered::<E>(target) {
            match self
                .run_listener_kind(kind, Value::from_arc(event.clone()))
                .await
            {
                Ok(Some(value)) => {
                    self.record_dispatch(start);
                    return Ok(Some(value));
                }
                Ok(None) => continue,
                Err(error) => {
                    self.record_dispatch(start);
                    return Err(error);
                }
            }
        }
        self.record_dispatch(start);
        Ok(None)
    }

    /// Ordered bail dispatch over synchronous listeners registered with
    /// [`EventBus::on_sync`] and its `once`/`with` variants.
    ///
    /// Returns the first `Ok(Some(value))` and stops. `Err` is fail-closed.
    pub fn bail_sync<E: Event>(&self, event: Arc<E>) -> Result<Option<Value>, EventError> {
        self.bail_sync_inner(None, event)
    }

    pub fn bail_sync_scoped<E: Event>(
        &self,
        target: &Context,
        event: Arc<E>,
    ) -> Result<Option<Value>, EventError> {
        self.bail_sync_inner(Some(target), event)
    }

    fn bail_sync_inner<E: Event>(
        &self,
        target: Option<&Context>,
        event: Arc<E>,
    ) -> Result<Option<Value>, EventError> {
        let start = Instant::now();
        for listener in self.snapshot_sync::<E>(target) {
            let listener_start = Instant::now();
            let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                listener(Value::from_arc(event.clone()))
            })) {
                Ok(Ok(Some(value))) => Ok(Some(value)),
                // The once-guard's "already fired" answer still counts as a
                // call — record metrics BEFORE continuing (it skipped them).
                Ok(Ok(None)) => {
                    self.record_listener(listener_start);
                    continue;
                }
                Ok(Err(error)) => Err(error),
                Err(_) => Err(EventError::Listener("listener panicked".into())),
            };
            self.record_listener(listener_start);
            match result {
                Ok(Some(value)) => {
                    self.record_dispatch(start);
                    return Ok(Some(value));
                }
                Ok(None) => continue,
                Err(error) => {
                    self.record_dispatch(start);
                    return Err(error);
                }
            }
        }
        self.record_dispatch(start);
        Ok(None)
    }

    pub async fn waterfall<E, F>(&self, event: Arc<E>, inner: F) -> Result<E::Output, EventError>
    where
        E: WaterfallEvent,
        F: Fn() -> BoxFuture<'static, Result<E::Output, EventError>> + Send + Sync + 'static,
    {
        self.waterfall_inner(None, event, inner).await
    }

    pub async fn waterfall_scoped<E, F>(
        &self,
        target: &Context,
        event: Arc<E>,
        inner: F,
    ) -> Result<E::Output, EventError>
    where
        E: WaterfallEvent,
        F: Fn() -> BoxFuture<'static, Result<E::Output, EventError>> + Send + Sync + 'static,
    {
        self.waterfall_inner(Some(target), event, inner).await
    }

    async fn waterfall_inner<E, F>(
        &self,
        target: Option<&Context>,
        event: Arc<E>,
        inner: F,
    ) -> Result<E::Output, EventError>
    where
        E: WaterfallEvent,
        F: Fn() -> BoxFuture<'static, Result<E::Output, EventError>> + Send + Sync + 'static,
    {
        let start = Instant::now();
        let erased_inner: Next<Value> = Arc::new(move || {
            let future = inner();
            Box::pin(async move {
                let output = future.await?;
                Ok(Value::new(output))
            })
        });
        let listeners = self.snapshot_waterfall::<E>(target);
        let chain = build_waterfall_chain(Value::from_arc(event), listeners, erased_inner);
        let timeout = *self
            .inner
            .waterfall_timeout
            .lock()
            .expect("waterfall timeout poisoned");
        let value = match timeout {
            Some(timeout) => tokio::time::timeout(timeout, chain())
                .await
                .map_err(|_| EventError::Timeout),
            None => Ok(chain().await),
        };
        // Failed/timed-out dispatches are exactly the interesting ones —
        // record them like every other dispatch path does.
        let value = match value {
            Ok(Ok(value)) => value,
            Ok(Err(error)) | Err(error) => {
                self.record_dispatch(start);
                return Err(error);
            }
        };
        let output = match value.downcast::<E::Output>() {
            Ok(output) => output,
            Err(_) => {
                self.record_dispatch(start);
                return Err(EventError::TypeMismatch(E::NAME));
            }
        };
        self.record_dispatch(start);
        Ok(output.as_ref().clone())
    }
}

impl DynamicEvents {
    pub fn on_waterfall<F>(&self, name: impl Into<String>, listener: F) -> EffectHandle
    where
        F: Fn(DynamicEvent, DynamicNext) -> BoxFuture<'static, Result<Option<Value>, EventError>>
            + Send
            + Sync
            + 'static,
    {
        let name = name.into();
        let listener: DynamicWaterfallListener = Arc::new(listener);
        let id = self.bus.inner.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut events = self
                .bus
                .inner
                .dynamic_waterfall
                .lock()
                .expect("dynamic event map poisoned");
            events
                .entry(name.clone())
                .or_default()
                .push((id, Arc::clone(&listener)));
        }
        let inner = Arc::clone(&self.bus.inner);
        EffectHandle::new(move || {
            if let Ok(mut events) = inner.dynamic_waterfall.lock() {
                if let Some(entry) = events.get_mut(&name) {
                    entry.retain(|(lid, _)| *lid != id);
                    if entry.is_empty() {
                        events.remove(&name);
                    }
                }
            }
        })
    }

    pub fn waterfall<F>(
        &self,
        name: impl Into<String>,
        args: Vec<Value>,
        inner: F,
    ) -> BoxFuture<'static, Result<Option<Value>, EventError>>
    where
        F: Fn() -> BoxFuture<'static, Result<Option<Value>, EventError>> + Send + Sync + 'static,
    {
        let name = name.into();
        let event = DynamicEvent {
            name: Arc::from(name),
            args: Arc::from(args),
        };
        let inner: DynamicNext = Arc::new(inner);
        let listeners = self
            .bus
            .inner
            .dynamic_waterfall
            .lock()
            .expect("dynamic event map poisoned")
            .get(event.name())
            .map(|entry| {
                entry
                    .iter()
                    .map(|(_, listener)| Arc::clone(listener))
                    .collect()
            })
            .unwrap_or_default();
        let chain = build_dynamic_waterfall_chain(event, listeners, inner);
        let timeout = *self
            .bus
            .inner
            .waterfall_timeout
            .lock()
            .expect("waterfall timeout poisoned");
        match timeout {
            Some(timeout) => Box::pin(async move {
                tokio::time::timeout(timeout, chain())
                    .await
                    .map_err(|_| EventError::Timeout)?
            }),
            None => chain(),
        }
    }
}

fn build_dynamic_waterfall_chain(
    event: DynamicEvent,
    listeners: Vec<DynamicWaterfallListener>,
    inner: DynamicNext,
) -> DynamicNext {
    let mut next = inner;
    for listener in listeners.into_iter().rev() {
        let previous = next.clone();
        let event = event.clone();
        let name = event.name().to_owned();
        next = Arc::new(move || {
            let listener = Arc::clone(&listener);
            let previous = Arc::clone(&previous);
            let event = event.clone();
            let name = name.clone();
            let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let called_clone = Arc::clone(&called);
            let wrapped_previous: DynamicNext = Arc::new(move || {
                called_clone.store(true, Ordering::SeqCst);
                let previous = Arc::clone(&previous);
                Box::pin(async move { previous().await })
            });
            Box::pin(async move {
                let result = listener(event.clone(), wrapped_previous).await;
                if !called.load(Ordering::SeqCst) {
                    tracing::warn!(event = %name, "dynamic waterfall listener returned without calling next()");
                }
                result
            })
        });
    }
    next
}

fn build_waterfall_chain(
    event: Value,
    listeners: Vec<ErasedWaterfallListener>,
    inner: Next<Value>,
) -> Next<Value> {
    let mut next = inner;
    for listener in listeners.into_iter().rev() {
        let previous = next.clone();
        let event = event.clone();
        next = Arc::new(move || {
            let listener = Arc::clone(&listener);
            let previous = Arc::clone(&previous);
            let event = event.clone();
            Box::pin(async move {
                match std::panic::AssertUnwindSafe(listener(event.clone(), previous))
                    .catch_unwind()
                    .await
                {
                    Ok(result) => result,
                    Err(_) => Err(EventError::Listener(
                        "typed waterfall listener panicked".into(),
                    )),
                }
            })
        });
    }
    next
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Clone)]
    struct Ping;
    impl Event for Ping {
        const NAME: &'static str = "demo/ping";
    }

    #[derive(Clone)]
    struct Transform;
    impl Event for Transform {
        const NAME: &'static str = "demo/transform";
    }
    impl WaterfallEvent for Transform {
        type Output = u64;
    }

    fn value_u64(v: u64) -> Value {
        Value::new(v)
    }

    #[tokio::test]
    async fn serial_collect_runs_listeners_in_order() {
        let bus = EventBus::new();
        let order = Arc::new(AtomicUsize::new(0));
        let o1 = Arc::clone(&order);
        let _h1 = bus.on::<Ping, _>(move |_| {
            let o = Arc::clone(&o1);
            Box::pin(async move {
                o.store(1, Ordering::SeqCst);
                Ok(value_u64(1))
            })
        });
        let o2 = Arc::clone(&order);
        let _h2 = bus.on::<Ping, _>(move |_| {
            let o = Arc::clone(&o2);
            Box::pin(async move {
                o.store(2, Ordering::SeqCst);
                Ok(value_u64(2))
            })
        });
        let results = bus.serial_collect(Arc::new(Ping)).await;
        assert_eq!(results.len(), 2);
        assert_eq!(order.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn serial_returns_first_value_and_short_circuits() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&calls);
        let _h1 = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c1);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(value_u64(10))
            })
        });
        let c2 = Arc::clone(&calls);
        let _h2 = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c2);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(value_u64(20))
            })
        });
        let got = bus.serial(Arc::new(Ping)).await.unwrap().unwrap();
        assert_eq!(*got.downcast::<u64>().unwrap(), 10);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn serial_with_no_listeners_returns_none() {
        let bus = EventBus::new();
        assert!(bus.serial::<Ping>(Arc::new(Ping)).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bail_returns_first_ok() {
        let bus = EventBus::new();
        let _h1 = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(10)) }));
        let _h2 = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(20)) }));
        let got = bus.bail(Arc::new(Ping)).await.unwrap().unwrap();
        assert_eq!(*got.downcast::<u64>().unwrap(), 10);
    }

    #[tokio::test]
    async fn emit_delivers_to_listener() {
        let bus = EventBus::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
        let _h = bus.on::<Ping, _>(move |_| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.send(()).await;
                Ok(value_u64(0))
            })
        });
        bus.emit(Arc::new(Ping));
        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn emit_sync_runs_sync_listeners_in_order() {
        let bus = EventBus::new();
        let order = Arc::new(AtomicUsize::new(0));
        let o1 = Arc::clone(&order);
        let _h1 = bus.on_sync::<Ping, _>(move |_| {
            o1.store(1, Ordering::SeqCst);
            Ok(value_u64(1))
        });
        let o2 = Arc::clone(&order);
        let _h2 = bus.on_sync::<Ping, _>(move |_| {
            o2.store(2, Ordering::SeqCst);
            Ok(value_u64(2))
        });

        let results = bus.emit_sync(Arc::new(Ping));
        assert_eq!(order.load(Ordering::SeqCst), 2);
        assert_eq!(results.len(), 2);
        assert_eq!(*results[0].as_ref().unwrap().downcast::<u64>().unwrap(), 1);
        assert_eq!(*results[1].as_ref().unwrap().downcast::<u64>().unwrap(), 2);
    }

    #[test]
    fn emit_sync_isolates_listener_errors_and_panics() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&calls);
        let _err = bus.on_sync::<Ping, _>(move |_| {
            c1.fetch_add(1, Ordering::SeqCst);
            Err(EventError::Listener("boom".into()))
        });
        let c2 = Arc::clone(&calls);
        let _panic = bus.on_sync::<Ping, _>(move |_| {
            c2.fetch_add(10, Ordering::SeqCst);
            panic!("sync listener panic");
        });

        let results = bus.emit_sync(Arc::new(Ping));
        assert_eq!(calls.load(Ordering::SeqCst), 11);
        assert_eq!(results.len(), 2);
        assert!(results[0].is_err());
        assert!(results[1].is_err());
    }

    #[test]
    fn emit_runs_sync_listeners_immediately() {
        let bus = EventBus::new();
        let ran = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&ran);
        let _h = bus.on_sync::<Ping, _>(move |_| {
            r.store(true, Ordering::SeqCst);
            Ok(value_u64(0))
        });

        bus.emit(Arc::new(Ping));
        assert!(ran.load(Ordering::SeqCst));
    }

    #[test]
    fn listener_count_counts_sync_listeners() {
        let bus = EventBus::new();
        let _h = bus.on_sync::<Ping, _>(|_| Ok(value_u64(0)));
        assert_eq!(bus.listener_count(Ping::NAME), 1);
    }

    #[tokio::test]
    async fn dropping_on_sync_handle_removes_listener() {
        let bus = EventBus::new();
        let h = bus.on_sync::<Ping, _>(|_| Ok(value_u64(1)));
        assert_eq!(bus.listener_count(Ping::NAME), 1);
        h.dispose_async().await;
        assert_eq!(bus.listener_count(Ping::NAME), 0);
        assert!(bus.emit_sync(Arc::new(Ping)).is_empty());
    }

    #[tokio::test]
    async fn parallel_runs_all_listeners_and_isolates_errors() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&count);
        let _h1 = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c1);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(value_u64(1))
            })
        });
        let c2 = Arc::clone(&count);
        let _h2 = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c2);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(EventError::Listener("boom".into()))
            })
        });
        let result = bus.parallel(Arc::new(Ping)).await;
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert!(matches!(result, Err(EventError::Aggregate(_))));
    }

    #[tokio::test]
    async fn dropping_on_handle_removes_listener() {
        let bus = EventBus::new();
        let h = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(1)) }));
        h.dispose_async().await;
        assert!(bus.serial(Arc::new(Ping)).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn serial_propagates_listener_error() {
        let bus = EventBus::new();
        let _h =
            bus.on::<Ping, _>(|_| Box::pin(async { Err(EventError::Listener("boom".into())) }));
        assert!(bus.serial(Arc::new(Ping)).await.is_err());
    }

    #[tokio::test]
    async fn bail_propagates_listener_error() {
        let bus = EventBus::new();
        let _h =
            bus.on::<Ping, _>(|_| Box::pin(async { Err(EventError::Listener("boom".into())) }));
        assert!(bus.bail(Arc::new(Ping)).await.is_err());
    }

    #[tokio::test]
    async fn bail_short_circuits_after_first_ok() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&calls);
        let _h1 = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c1);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(value_u64(1))
            })
        });
        let c2 = Arc::clone(&calls);
        let _h2 = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c2);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(value_u64(2))
            })
        });
        let _ = bus.bail(Arc::new(Ping)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn bail_with_no_listeners_returns_none() {
        let bus = EventBus::new();
        assert!(bus.bail::<Ping>(Arc::new(Ping)).await.unwrap().is_none());
    }

    #[test]
    fn bail_sync_returns_first_value() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&calls);
        let _h1 = bus.on_sync::<Ping, _>(move |_| {
            c1.fetch_add(1, Ordering::SeqCst);
            Ok(value_u64(10))
        });
        let c2 = Arc::clone(&calls);
        let _h2 = bus.on_sync::<Ping, _>(move |_| {
            c2.fetch_add(1, Ordering::SeqCst);
            Ok(value_u64(20))
        });
        let got = bus.bail_sync(Arc::new(Ping)).unwrap().unwrap();
        assert_eq!(*got.downcast::<u64>().unwrap(), 10);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bail_sync_propagates_error() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&calls);
        let _err = bus.on_sync::<Ping, _>(move |_| {
            c1.fetch_add(1, Ordering::SeqCst);
            Err(EventError::Listener("boom".into()))
        });
        let c2 = Arc::clone(&calls);
        let _ok = bus.on_sync::<Ping, _>(move |_| {
            c2.fetch_add(10, Ordering::SeqCst);
            Ok(value_u64(0))
        });
        assert!(bus.bail_sync(Arc::new(Ping)).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn parallel_with_no_listeners_is_ok() {
        let bus = EventBus::new();
        assert!(bus.parallel::<Ping>(Arc::new(Ping)).await.is_ok());
    }

    #[tokio::test]
    async fn emit_with_no_listeners_does_not_panic() {
        let bus = EventBus::new();
        bus.emit(Arc::new(Ping));
    }

    #[tokio::test]
    async fn serial_collect_isolates_listener_panic() {
        let bus = EventBus::new();
        let order = Arc::new(AtomicUsize::new(0));
        let o1 = Arc::clone(&order);
        let _panic = bus.on::<Ping, _>(move |_| {
            let o = Arc::clone(&o1);
            Box::pin(async move {
                o.fetch_add(1, Ordering::SeqCst);
                panic!("listener panic");
            })
        });
        let o2 = Arc::clone(&order);
        let _ok = bus.on::<Ping, _>(move |_| {
            let o = Arc::clone(&o2);
            Box::pin(async move {
                o.fetch_add(10, Ordering::SeqCst);
                Ok(value_u64(0))
            })
        });
        let results = bus.serial_collect(Arc::new(Ping)).await;
        assert_eq!(order.load(Ordering::SeqCst), 11);
        assert_eq!(results.iter().filter(|r| r.is_err()).count(), 1);
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    }

    #[tokio::test]
    async fn serial_fails_closed_on_listener_error() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&calls);
        let _err = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c1);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(EventError::Listener("boom".into()))
            })
        });
        let c2 = Arc::clone(&calls);
        let _ok = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c2);
            Box::pin(async move {
                c.fetch_add(10, Ordering::SeqCst);
                Ok(value_u64(0))
            })
        });
        assert!(bus.serial(Arc::new(Ping)).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn parallel_isolates_listener_panic() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&count);
        let _panic = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c1);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                panic!("listener panic");
            })
        });
        let c2 = Arc::clone(&count);
        let _ok = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c2);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(value_u64(0))
            })
        });
        let result = bus.parallel(Arc::new(Ping)).await;
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert!(matches!(result, Err(EventError::Aggregate(_))));
    }

    #[tokio::test]
    async fn serial_strict_fails_fast() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&calls);
        let _h1 = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c1);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(EventError::Listener("boom".into()))
            })
        });
        let c2 = Arc::clone(&calls);
        let _h2 = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c2);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(value_u64(0))
            })
        });
        let result = bus.serial_strict(Arc::new(Ping)).await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn parallel_aggregates_errors() {
        let bus = EventBus::new();
        let _h1 = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(1)) }));
        let _h2 =
            bus.on::<Ping, _>(|_| Box::pin(async { Err(EventError::Listener("boom".into())) }));
        assert!(matches!(
            bus.parallel(Arc::new(Ping)).await,
            Err(EventError::Aggregate(ref errors)) if errors.len() == 1
        ));
    }

    #[tokio::test]
    async fn emit_with_zero_limit_drops_listener() {
        let bus = EventBus::with_emit_limit(0);
        let ran = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&ran);
        let _h = bus.on::<Ping, _>(move |_| {
            let r = Arc::clone(&r);
            Box::pin(async move {
                r.store(true, Ordering::SeqCst);
                Ok(value_u64(0))
            })
        });
        bus.emit(Arc::new(Ping));
        tokio::task::yield_now().await;
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(bus.dropped_emits(), 1);
    }

    #[tokio::test]
    async fn emit_in_flight_counts_held_permits() {
        let bus = EventBus::with_emit_limit(2);
        let release = Arc::new(tokio::sync::Notify::new());
        let n = Arc::clone(&release);
        let _h = bus.on::<Ping, _>(move |_| {
            let n = Arc::clone(&n);
            Box::pin(async move {
                n.notified().await;
                Ok(value_u64(0))
            })
        });
        bus.emit(Arc::new(Ping));
        assert_eq!(bus.emit_in_flight(), 1);
        release.notify_waiters();
    }

    #[tokio::test]
    async fn serial_strict_success_returns_values() {
        let bus = EventBus::new();
        let _h = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(1)) }));
        let values = bus.serial_strict(Arc::new(Ping)).await.unwrap();
        assert_eq!(*values[0].downcast::<u64>().unwrap(), 1);
    }

    #[tokio::test]
    async fn serial_strict_multiple_success() {
        let bus = EventBus::new();
        let _h1 = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(1)) }));
        let _h2 = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(2)) }));
        let values = bus.serial_strict(Arc::new(Ping)).await.unwrap();
        assert_eq!(values.len(), 2);
    }

    #[tokio::test]
    async fn parallel_success_returns_ok() {
        let bus = EventBus::new();
        let _h1 = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(1)) }));
        let _h2 = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(2)) }));
        assert!(bus.parallel(Arc::new(Ping)).await.is_ok());
    }

    #[tokio::test]
    async fn parallel_with_all_ok_listeners_succeeds() {
        let bus = EventBus::new();
        let _h1 = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(1)) }));
        let _h2 = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(2)) }));
        assert!(bus.parallel(Arc::new(Ping)).await.is_ok());
    }

    #[tokio::test]
    async fn emit_delivers_to_all_listeners() {
        let bus = EventBus::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(2);
        let tx1 = tx.clone();
        let _h1 = bus.on::<Ping, _>(move |_| {
            let tx = tx1.clone();
            Box::pin(async move {
                let _ = tx.send(()).await;
                Ok(value_u64(0))
            })
        });
        let tx2 = tx.clone();
        let _h2 = bus.on::<Ping, _>(move |_| {
            let tx = tx2.clone();
            Box::pin(async move {
                let _ = tx.send(()).await;
                Ok(value_u64(0))
            })
        });
        bus.emit(Arc::new(Ping));
        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn emit_limit_reports_configured_limit() {
        assert_eq!(EventBus::new().emit_limit(), 1024);
        assert_eq!(EventBus::with_emit_limit(3).emit_limit(), 3);
    }

    #[test]
    fn listener_count_reports_registered_listeners() {
        let bus = EventBus::new();
        assert_eq!(bus.listener_count(Ping::NAME), 0);
        assert_eq!(bus.listener_count(Transform::NAME), 0);
        let _h1 = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(0)) }));
        let _h2 = bus.on_waterfall::<Transform, _>(|_, next| Box::pin(async move { next().await }));
        assert_eq!(bus.listener_count(Ping::NAME), 1);
        assert_eq!(bus.listener_count(Transform::NAME), 1);
    }

    #[tokio::test]
    async fn dispatch_and_listener_metrics_accumulate() {
        let bus = EventBus::new();
        assert_eq!(bus.listener_calls(), 0);
        assert_eq!(bus.dispatch_calls(), 0);
        assert_eq!(bus.listener_avg_ns(), 0);
        assert_eq!(bus.dispatch_avg_ns(), 0);

        let _h = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(1)) }));
        let _ = bus.serial(Arc::new(Ping)).await;
        assert!(bus.listener_calls() >= 1);
        assert!(bus.dispatch_calls() >= 1);
    }

    #[tokio::test]
    async fn emit_mixed_sync_and_async_listeners() {
        let bus = EventBus::new();
        let sync_ran = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&sync_ran);
        let _sync = bus.on_sync::<Ping, _>(move |_| {
            s.store(true, Ordering::SeqCst);
            Ok(value_u64(1))
        });

        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
        let async_ran = Arc::new(AtomicBool::new(false));
        let a = Arc::clone(&async_ran);
        let _async = bus.on::<Ping, _>(move |_| {
            let tx = tx.clone();
            let a = Arc::clone(&a);
            Box::pin(async move {
                a.store(true, Ordering::SeqCst);
                let _ = tx.send(()).await;
                Ok(value_u64(2))
            })
        });

        bus.emit(Arc::new(Ping));
        assert!(sync_ran.load(Ordering::SeqCst));
        assert!(!async_ran.load(Ordering::SeqCst));

        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(async_ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn serial_runs_sync_listeners_in_registration_order() {
        let bus = EventBus::new();
        let order = Arc::new(AtomicUsize::new(0));

        let o1 = Arc::clone(&order);
        let _sync = bus.on_sync::<Ping, _>(move |_| {
            o1.store(1, Ordering::SeqCst);
            Ok(value_u64(7))
        });
        let o2 = Arc::clone(&order);
        let _async = bus.on::<Ping, _>(move |_| {
            let o = Arc::clone(&o2);
            Box::pin(async move {
                o.store(2, Ordering::SeqCst);
                Ok(value_u64(6))
            })
        });

        let got = bus.serial(Arc::new(Ping)).await.unwrap().unwrap();
        assert_eq!(*got.downcast::<u64>().unwrap(), 7);
        assert_eq!(order.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn parallel_includes_sync_and_async_listeners() {
        let bus = EventBus::new();
        let _sync = bus.on_sync::<Ping, _>(|_| Ok(value_u64(7)));
        let _async = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(6)) }));

        assert!(bus.parallel(Arc::new(Ping)).await.is_ok());
    }

    #[tokio::test]
    async fn bail_includes_sync_listeners() {
        let bus = EventBus::new();
        let _sync = bus.on_sync::<Ping, _>(|_| Ok(value_u64(7)));
        let _async = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(6)) }));

        let got = bus.bail(Arc::new(Ping)).await.unwrap().unwrap();
        assert_eq!(*got.downcast::<u64>().unwrap(), 7);
    }

    #[tokio::test]
    async fn serial_collect_includes_sync_listeners() {
        let bus = EventBus::new();
        let _sync = bus.on_sync::<Ping, _>(|_| Ok(value_u64(7)));
        let _async = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(6)) }));

        let results = bus.serial_collect(Arc::new(Ping)).await;
        assert_eq!(results.len(), 2);
        assert_eq!(*results[0].as_ref().unwrap().downcast::<u64>().unwrap(), 7);
        assert_eq!(*results[1].as_ref().unwrap().downcast::<u64>().unwrap(), 6);
    }

    #[tokio::test]
    async fn prepend_sync_listener_runs_before_existing_async() {
        let bus = EventBus::new();
        let _async = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(6)) }));
        let _sync = bus.on_sync_with::<Ping, _>(
            |_| Ok(value_u64(9)),
            EventOptions {
                prepend: true,
                global: false,
            },
        );

        let got = bus.serial(Arc::new(Ping)).await.unwrap().unwrap();
        assert_eq!(*got.downcast::<u64>().unwrap(), 9);
    }

    #[tokio::test]
    async fn strict_variants_include_sync_listeners() {
        let bus = EventBus::new();
        let _sync = bus.on_sync::<Ping, _>(|_| Ok(value_u64(7)));
        let _async = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(6)) }));

        let serial_values = bus.serial_strict(Arc::new(Ping)).await.unwrap();
        assert_eq!(serial_values.len(), 2);
        assert_eq!(*serial_values[0].downcast::<u64>().unwrap(), 7);
        assert_eq!(*serial_values[1].downcast::<u64>().unwrap(), 6);

        assert!(bus.parallel(Arc::new(Ping)).await.is_ok());
    }

    #[tokio::test]
    async fn emit_isolates_async_listener_errors() {
        let bus = EventBus::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(2);

        let tx1 = tx.clone();
        let _err = bus.on::<Ping, _>(move |_| {
            let tx = tx1.clone();
            Box::pin(async move {
                let _ = tx.send(()).await;
                Err(EventError::Listener("boom".into()))
            })
        });
        let tx2 = tx.clone();
        let _ok = bus.on::<Ping, _>(move |_| {
            let tx = tx2.clone();
            Box::pin(async move {
                let _ = tx.send(()).await;
                Ok(value_u64(0))
            })
        });

        bus.emit(Arc::new(Ping));
        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn emit_with_zero_limit_still_runs_sync_listeners() {
        let bus = EventBus::with_emit_limit(0);
        let sync_ran = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&sync_ran);
        let _sync = bus.on_sync::<Ping, _>(move |_| {
            s.store(true, Ordering::SeqCst);
            Ok(value_u64(1))
        });

        let async_ran = Arc::new(AtomicBool::new(false));
        let a = Arc::clone(&async_ran);
        let _async = bus.on::<Ping, _>(move |_| {
            let a = Arc::clone(&a);
            Box::pin(async move {
                a.store(true, Ordering::SeqCst);
                Ok(value_u64(2))
            })
        });

        bus.emit(Arc::new(Ping));
        assert!(sync_ran.load(Ordering::SeqCst));
        assert!(!async_ran.load(Ordering::SeqCst));
        assert_eq!(bus.dropped_emits(), 1);
    }

    #[test]
    fn emit_sync_with_no_listeners_is_empty() {
        let bus = EventBus::new();
        assert!(bus.emit_sync(Arc::new(Ping)).is_empty());
    }

    #[test]
    fn emit_sync_records_listener_and_dispatch_metrics() {
        let bus = EventBus::new();
        assert_eq!(bus.listener_calls(), 0);
        assert_eq!(bus.dispatch_calls(), 0);
        let _h = bus.on_sync::<Ping, _>(|_| Ok(value_u64(1)));
        let _ = bus.emit_sync(Arc::new(Ping));
        assert_eq!(bus.listener_calls(), 1);
        assert_eq!(bus.dispatch_calls(), 1);
    }

    #[tokio::test]
    async fn parallel_aggregates_listener_panic() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&count);
        let _panic = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c1);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                panic!("parallel strict panic");
            })
        });
        let c2 = Arc::clone(&count);
        let _ok = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c2);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(value_u64(0))
            })
        });

        let result = bus.parallel(Arc::new(Ping)).await;
        assert!(matches!(result, Err(EventError::Aggregate(_))));
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn serial_strict_fails_fast_on_panic() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&calls);
        let _panic = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c1);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                panic!("serial strict panic");
            })
        });
        let c2 = Arc::clone(&calls);
        let _ok = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c2);
            Box::pin(async move {
                c.fetch_add(10, Ordering::SeqCst);
                Ok(value_u64(0))
            })
        });

        let result = bus.serial_strict(Arc::new(Ping)).await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn listener_count_tracks_all_kinds_and_removal() {
        let bus = EventBus::new();
        let sync = bus.on_sync::<Ping, _>(|_| Ok(value_u64(0)));
        let async_listener = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(0)) }));
        let waterfall =
            bus.on_waterfall::<Transform, _>(|_, next| Box::pin(async move { next().await }));

        assert_eq!(bus.listener_count(Ping::NAME), 2);
        assert_eq!(bus.listener_count(Transform::NAME), 1);

        sync.dispose_async().await;
        assert_eq!(bus.listener_count(Ping::NAME), 1);
        async_listener.dispose_async().await;
        assert_eq!(bus.listener_count(Ping::NAME), 0);
        waterfall.dispose_async().await;
        assert_eq!(bus.listener_count(Transform::NAME), 0);
    }

    #[tokio::test]
    async fn typed_waterfall_is_onion_middleware() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));

        let c1 = Arc::clone(&calls);
        let _outer = bus.on_waterfall::<Transform, _>(move |_, next| {
            let c = Arc::clone(&c1);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                let value = next().await?;
                Ok(value + 1)
            })
        });

        let c2 = Arc::clone(&calls);
        let _inner = bus.on_waterfall::<Transform, _>(move |_, next| {
            let c = Arc::clone(&c2);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                let value = next().await?;
                Ok(value * 10)
            })
        });

        let out = bus
            .waterfall(Arc::new(Transform), || Box::pin(async { Ok(2u64) }))
            .await
            .unwrap();

        assert_eq!(out, 21);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn typed_waterfall_short_circuits() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&calls);
        let _outer = bus.on_waterfall::<Transform, _>(move |_, _next| {
            let c = Arc::clone(&c1);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(99u64)
            })
        });
        let c2 = Arc::clone(&calls);
        let _later = bus.on_waterfall::<Transform, _>(move |_, next| {
            let c = Arc::clone(&c2);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                next().await
            })
        });

        let out = bus
            .waterfall(Arc::new(Transform), || Box::pin(async { Ok(1u64) }))
            .await
            .unwrap();

        assert_eq!(out, 99);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn typed_waterfall_propagates_error() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&calls);
        let _outer = bus.on_waterfall::<Transform, _>(move |_, next| {
            let c = Arc::clone(&c1);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                let _ = next().await?;
                Err(EventError::Listener("boom".into()))
            })
        });
        let c2 = Arc::clone(&calls);
        let _later = bus.on_waterfall::<Transform, _>(move |_, next| {
            let c = Arc::clone(&c2);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                next().await
            })
        });

        let result = bus
            .waterfall(Arc::new(Transform), || Box::pin(async { Ok(1u64) }))
            .await;

        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn typed_waterfall_times_out() {
        let bus = EventBus::new();
        bus.set_waterfall_timeout(Duration::from_millis(5));
        let _hang = bus.on_waterfall::<Transform, _>(|_, _next| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(1u64)
            })
        });

        let result = bus
            .waterfall(Arc::new(Transform), || Box::pin(async { Ok(1u64) }))
            .await;

        assert!(matches!(result, Err(EventError::Timeout)));
    }

    #[tokio::test]
    async fn typed_waterfall_without_listeners_returns_inner() {
        let bus = EventBus::new();
        let out = bus
            .waterfall(Arc::new(Transform), || Box::pin(async { Ok(42u64) }))
            .await
            .unwrap();
        assert_eq!(out, 42);
    }

    #[tokio::test]
    async fn typed_waterfall_isolates_panic_as_error() {
        let bus = EventBus::new();
        let _h = bus.on_waterfall::<Transform, _>(|_, _next| {
            Box::pin(async move { panic!("typed waterfall panic") })
        });
        let result = bus
            .waterfall(Arc::new(Transform), || Box::pin(async { Ok(1u64) }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dropping_typed_waterfall_handle_removes_listener() {
        let bus = EventBus::new();
        let h = bus.on_waterfall::<Transform, _>(|_, next| Box::pin(async move { next().await }));
        h.dispose_async().await;
        let out = bus
            .waterfall(Arc::new(Transform), || Box::pin(async { Ok(5u64) }))
            .await
            .unwrap();
        assert_eq!(out, 5);
    }

    #[tokio::test]
    async fn dynamic_waterfall_uses_next_continuation() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&calls);
        let _outer = bus
            .dynamic()
            .on_waterfall("demo/dynamic", move |event, next| {
                let c = Arc::clone(&c1);
                Box::pin(async move {
                    assert_eq!(event.args()[0].downcast::<u64>().unwrap().as_ref(), &1);
                    c.fetch_add(1, Ordering::SeqCst);
                    let value = next().await?;
                    Ok(value.map(|v| Value::new(*v.downcast::<u64>().unwrap() + 1)))
                })
            });
        let c2 = Arc::clone(&calls);
        let _inner = bus
            .dynamic()
            .on_waterfall("demo/dynamic", move |_event, next| {
                let c = Arc::clone(&c2);
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    next().await
                })
            });

        let out = bus
            .dynamic()
            .waterfall("demo/dynamic", vec![Value::new(1u64)], || {
                Box::pin(async { Ok(Some(Value::new(10u64))) })
            })
            .await
            .unwrap()
            .unwrap();

        assert_eq!(*out.downcast::<u64>().unwrap(), 11);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn dynamic_waterfall_short_circuits_without_next() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&calls);
        let _outer = bus
            .dynamic()
            .on_waterfall("demo/dynamic", move |_event, _next| {
                let c = Arc::clone(&c1);
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(Some(Value::new(99u64)))
                })
            });
        let c2 = Arc::clone(&calls);
        let _later = bus
            .dynamic()
            .on_waterfall("demo/dynamic", move |_event, next| {
                let c = Arc::clone(&c2);
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    next().await
                })
            });

        let out = bus
            .dynamic()
            .waterfall("demo/dynamic", vec![Value::new(1u64)], || {
                Box::pin(async { Ok(Some(Value::new(10u64))) })
            })
            .await
            .unwrap()
            .unwrap();

        assert_eq!(*out.downcast::<u64>().unwrap(), 99);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn once_listener_runs_only_once() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let _h = bus.once::<Ping, _>(move |_| {
            let c = Arc::clone(&c);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(value_u64(1))
            })
        });

        let results = bus.serial_collect(Arc::new(Ping)).await;
        assert_eq!(results.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(bus.listener_count(Ping::NAME), 0);

        let results = bus.serial_collect(Arc::new(Ping)).await;
        assert!(results.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn once_listener_stays_registered_when_skipped_by_earlier_error() {
        let bus = EventBus::new();

        let calls = Arc::new(AtomicUsize::new(0));
        let c1 = Arc::clone(&calls);
        let h_err = bus.on::<Ping, _>(move |_| {
            let c = Arc::clone(&c1);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(EventError::Listener("boom".into()))
            })
        });

        let c2 = Arc::clone(&calls);
        let _once = bus.once::<Ping, _>(move |_| {
            let c = Arc::clone(&c2);
            Box::pin(async move {
                c.fetch_add(10, Ordering::SeqCst);
                Ok(value_u64(1))
            })
        });

        let results = bus.serial(Arc::new(Ping)).await;
        assert!(results.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(bus.listener_count(Ping::NAME), 2);

        h_err.dispose_async().await;
        let got = bus.serial(Arc::new(Ping)).await.unwrap().unwrap();
        assert_eq!(*got.downcast::<u64>().unwrap(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 11);
        assert_eq!(bus.listener_count(Ping::NAME), 0);

        assert!(bus.serial(Arc::new(Ping)).await.unwrap().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 11);
    }

    #[tokio::test]
    async fn prepend_option_runs_async_listener_first() {
        let bus = EventBus::new();
        let _h1 = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(1)) }));
        let _h2 = bus.on_with::<Ping, _>(
            |_| Box::pin(async { Ok(value_u64(2)) }),
            EventOptions {
                prepend: true,
                global: false,
            },
        );

        let got = bus.serial(Arc::new(Ping)).await.unwrap().unwrap();
        assert_eq!(*got.downcast::<u64>().unwrap(), 2);
    }

    #[test]
    fn prepend_option_runs_sync_listener_first() {
        let bus = EventBus::new();
        let order = Arc::new(AtomicUsize::new(0));

        let o1 = Arc::clone(&order);
        let _h1 = bus.on_sync::<Ping, _>(move |_| {
            o1.store(1, Ordering::SeqCst);
            Ok(value_u64(1))
        });

        let o2 = Arc::clone(&order);
        let _h2 = bus.on_sync_with::<Ping, _>(
            move |_| {
                o2.store(2, Ordering::SeqCst);
                Ok(value_u64(2))
            },
            EventOptions {
                prepend: true,
                global: false,
            },
        );

        let _ = bus.emit_sync(Arc::new(Ping));
        assert_eq!(order.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn once_sync_listener_runs_only_once() {
        let bus = EventBus::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let _h = bus.once_sync::<Ping, _>(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(value_u64(1))
        });

        let results = bus.emit_sync(Arc::new(Ping));
        assert_eq!(results.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(bus.listener_count(Ping::NAME), 0);

        let results = bus.emit_sync(Arc::new(Ping));
        assert!(results.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn async_aliases_register_listeners() {
        let bus = EventBus::new();
        let _h1 = bus.on_async::<Ping, _>(|_| Box::pin(async { Ok(value_u64(1)) }));
        let _h2 = bus.once_async::<Ping, _>(|_| Box::pin(async { Ok(value_u64(2)) }));
        let _h3 = bus.on_async_with::<Ping, _>(
            |_| Box::pin(async { Ok(value_u64(3)) }),
            EventOptions::default(),
        );
        let _h4 = bus.once_async_with::<Ping, _>(
            |_| Box::pin(async { Ok(value_u64(4)) }),
            EventOptions::default(),
        );

        assert_eq!(bus.listener_count(Ping::NAME), 4);

        let results = bus.serial_collect(Arc::new(Ping)).await;
        assert_eq!(results.len(), 4);
        assert_eq!(bus.listener_count(Ping::NAME), 2);

        let results = bus.serial_collect(Arc::new(Ping)).await;
        assert_eq!(results.len(), 2);
        assert_eq!(bus.listener_count(Ping::NAME), 2);
    }

    crate::cordis_event!(MacroEvent, "demo/macro-event", {
        count: u64,
        label: String,
    });

    crate::cordis_event!(MacroMarker, "demo/macro-marker", {});

    crate::cordis_event!(pub(crate) MacroVisibleEvent, "demo/macro-visible", {
        n: u32,
    });

    #[test]
    fn event_macro_sets_name_and_fields() {
        assert_eq!(MacroEvent::NAME, "demo/macro-event");
        let event = MacroEvent {
            count: 7,
            label: "hello".to_owned(),
        };
        assert_eq!(event.count, 7);
        assert_eq!(event.label, "hello");
    }

    #[test]
    fn event_macro_supports_marker_events() {
        assert_eq!(MacroMarker::NAME, "demo/macro-marker");
        let _marker = MacroMarker {};
    }

    #[test]
    fn event_macro_supports_visibility_qualifier() {
        assert_eq!(MacroVisibleEvent::NAME, "demo/macro-visible");
        let event = MacroVisibleEvent { n: 3 };
        assert_eq!(event.n, 3);
    }

    #[tokio::test]
    async fn event_macro_type_dispatches_through_bus() {
        let bus = EventBus::new();
        let _h = bus.on::<MacroEvent, _>(|event| {
            Box::pin(async move { Ok(Value::new(event.count + event.label.len() as u64)) })
        });

        let got = bus
            .serial(Arc::new(MacroEvent {
                count: 2,
                label: "abc".to_owned(),
            }))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*got.downcast::<u64>().unwrap(), 5);
    }

    #[tokio::test]
    async fn target_filter_controls_owned_listeners() {
        let bus = EventBus::new();
        let owner = Context::new().with_event_bus(&bus);
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        let _h = owner.on::<Ping, _>(move |_| {
            let h = Arc::clone(&h);
            Box::pin(async move {
                h.fetch_add(1, Ordering::SeqCst);
                Ok(value_u64(0))
            })
        });

        let allow = Context::new().with_filter(|_ctx| true);
        let deny = Context::new().with_filter(|_ctx| false);

        let _ = bus.serial_scoped(&allow, Arc::new(Ping)).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let _ = bus.serial_scoped(&deny, Arc::new(Ping)).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn global_option_bypasses_target_filter() {
        let bus = EventBus::new();
        let owner = Context::new().with_event_bus(&bus);
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        let _h = owner.on_with::<Ping, _>(
            move |_| {
                let h = Arc::clone(&h);
                Box::pin(async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    Ok(value_u64(0))
                })
            },
            EventOptions {
                prepend: false,
                global: true,
            },
        );

        let deny = Context::new().with_filter(|_ctx| false);
        let _ = bus.serial_scoped(&deny, Arc::new(Ping)).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unowned_listener_is_global_under_filter() {
        let bus = EventBus::new();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        let _h = bus.on::<Ping, _>(move |_| {
            let h = Arc::clone(&h);
            Box::pin(async move {
                h.fetch_add(1, Ordering::SeqCst);
                Ok(value_u64(0))
            })
        });

        let deny = Context::new().with_filter(|_ctx| false);
        let _ = bus.serial_scoped(&deny, Arc::new(Ping)).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn waterfall_scoped_filters_owned_waterfall_listeners() {
        let bus = EventBus::new();
        let owner = Context::new().with_event_bus(&bus);
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let _h = owner.on_waterfall::<Transform, _>(move |_, next| {
            let c = Arc::clone(&c);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                next().await
            })
        });

        let deny = Context::new().with_filter(|_ctx| false);
        let out = bus
            .waterfall_scoped(&deny, Arc::new(Transform), || Box::pin(async { Ok(1u64) }))
            .await
            .unwrap();
        assert_eq!(out, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let allow = Context::new().with_filter(|_ctx| true);
        let _ = bus
            .waterfall_scoped(&allow, Arc::new(Transform), || Box::pin(async { Ok(1u64) }))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn event_catalog_lists_names_and_type_names() {
        crate::cordis_event!(CatalogA, "demo/catalog-a", {});
        crate::cordis_event!(CatalogB, "demo/catalog-b", {});

        crate::event_catalog!(catalog, {
            "demo/catalog-a" => CatalogA,
            "demo/catalog-b" => CatalogB,
        });

        let catalog = catalog();
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0].0, "demo/catalog-a");
        assert!(catalog[0].1.contains("CatalogA"));
        assert_eq!(catalog[1].0, "demo/catalog-b");
        assert!(catalog[1].1.contains("CatalogB"));
    }

    #[test]
    fn dynamic_event_arg_accessors_and_errors() {
        let event = DynamicEvent {
            name: Arc::from("demo/dynamic-arg"),
            args: Arc::from(vec![Value::new(1u64), Value::new("payload".to_owned())]),
        };

        assert_eq!(event.name(), "demo/dynamic-arg");
        assert_eq!(event.args().len(), 2);
        assert_eq!(*event.arg::<u64>(0).unwrap().unwrap(), 1);
        assert!(matches!(event.arg::<u64>(1), Err(EventError::Cast(_))));
        assert!(event.arg::<u64>(2).unwrap().is_none());
    }

    #[test]
    fn event_bus_default_uses_default_emit_limit() {
        let bus = EventBus::default();
        assert_eq!(bus.emit_limit(), 1024);
    }

    #[tokio::test]
    async fn emit_scoped_filters_owned_async_listener() {
        let bus = EventBus::new();
        let owner = Context::new().with_event_bus(&bus);
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        let _h = owner.on::<Ping, _>(move |_| {
            let h = Arc::clone(&h);
            Box::pin(async move {
                h.fetch_add(1, Ordering::SeqCst);
                Ok(value_u64(0))
            })
        });

        let allow = Context::new().with_filter(|_ctx| true);
        let deny = Context::new().with_filter(|_ctx| false);

        bus.emit_scoped(&allow, Arc::new(Ping));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        bus.emit_scoped(&deny, Arc::new(Ping));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn emit_sync_scoped_filters_owned_sync_listener() {
        let bus = EventBus::new();
        let owner = Context::new().with_event_bus(&bus);
        let _h = owner.on_sync::<Ping, _>(|_| Ok(value_u64(7)));

        let allow = Context::new().with_filter(|_ctx| true);
        let deny = Context::new().with_filter(|_ctx| false);

        let allowed = bus.emit_sync_scoped(&allow, Arc::new(Ping));
        assert_eq!(allowed.len(), 1);
        assert_eq!(*allowed[0].as_ref().unwrap().downcast::<u64>().unwrap(), 7);

        assert!(bus.emit_sync_scoped(&deny, Arc::new(Ping)).is_empty());
    }

    #[tokio::test]
    async fn target_dispatch_variants_run_without_filter() {
        let bus = EventBus::new();
        let target = Context::new();
        let _sync = bus.on_sync::<Ping, _>(|_| Ok(value_u64(7)));
        let _async = bus.on::<Ping, _>(|_| Box::pin(async { Ok(value_u64(6)) }));

        let collected = bus.serial_collect_scoped(&target, Arc::new(Ping)).await;
        assert_eq!(collected.len(), 2);

        let strict = bus
            .serial_strict_scoped(&target, Arc::new(Ping))
            .await
            .unwrap();
        assert_eq!(strict.len(), 2);

        assert!(bus.parallel_scoped(&target, Arc::new(Ping)).await.is_ok());

        let bail = bus
            .bail_scoped(&target, Arc::new(Ping))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*bail.downcast::<u64>().unwrap(), 7);

        let bail_sync = bus
            .bail_sync_scoped(&target, Arc::new(Ping))
            .unwrap()
            .unwrap();
        assert_eq!(*bail_sync.downcast::<u64>().unwrap(), 7);
    }

    #[tokio::test]
    async fn dynamic_waterfall_times_out() {
        let bus = EventBus::new();
        bus.set_waterfall_timeout(Duration::from_millis(5));
        let _hang = bus
            .dynamic()
            .on_waterfall("demo/dynamic-timeout", |_event, _next| {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    Ok(Some(Value::new(1u64)))
                })
            });

        let result = bus
            .dynamic()
            .waterfall("demo/dynamic-timeout", vec![], || {
                Box::pin(async { Ok(Some(Value::new(2u64))) })
            })
            .await;
        assert!(matches!(result, Err(EventError::Timeout)));
    }

    #[tokio::test]
    async fn dropping_dynamic_waterfall_handle_removes_listener() {
        let bus = EventBus::new();
        let handle = bus
            .dynamic()
            .on_waterfall("demo/dynamic-dispose", |_event, next| {
                Box::pin(async move { next().await })
            });
        handle.dispose_async().await;

        let out = bus
            .dynamic()
            .waterfall("demo/dynamic-dispose", vec![], || {
                Box::pin(async { Ok(Some(Value::new(5u64))) })
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*out.downcast::<u64>().unwrap(), 5);
    }
}
