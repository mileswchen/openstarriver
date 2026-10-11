//! Framework abnormal-path integration tests over the public cordis-rslite
//! API: event dispatch failures, scoped filtering, service table errors,
//! fiber effect ordering, and emit saturation.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use cordis_rslite::{
    Context, EffectHandle, Event, EventBus, EventError, Fiber, Service, TypedService, Value,
    WaterfallEvent,
};

struct Ping;
impl Event for Ping {
    const NAME: &'static str = "abnormal/ping";
}

struct Transform;
impl Event for Transform {
    const NAME: &'static str = "abnormal/transform";
}
impl WaterfallEvent for Transform {
    type Output = u64;
}

struct Counter;
impl TypedService for Counter {
    const NAME: &'static str = "counter";
}

#[tokio::test]
async fn waterfall_listener_panic_is_contained() {
    let bus = EventBus::new();
    let _h =
        bus.on_waterfall::<Transform, _>(|_, _next| Box::pin(async { panic!("waterfall panic") }));
    let result = bus
        .waterfall(Arc::new(Transform), || Box::pin(async { Ok(1u64) }))
        .await;
    assert!(
        result.is_err(),
        "a panicking waterfall listener must not crash the dispatch"
    );
}

#[tokio::test]
async fn serial_listener_error_propagates() {
    let bus = EventBus::new();
    let _h = bus.on::<Ping, _>(|_| Box::pin(async { Err(EventError::Listener("boom".into())) }));
    let result = bus.serial(Arc::new(Ping)).await;
    assert!(matches!(result, Err(EventError::Listener(_))));
}

#[tokio::test]
async fn scoped_dispatch_runs_global_and_skips_foreign_owned() {
    let bus = EventBus::new();
    let global = Arc::new(AtomicUsize::new(0));
    let a_hits = Arc::new(AtomicUsize::new(0));
    let b_hits = Arc::new(AtomicUsize::new(0));

    let global_counter = Arc::clone(&global);
    let _g = bus.on::<Ping, _>(move |_| {
        let counter = Arc::clone(&global_counter);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Value::new(()))
        })
    });

    let owner_a = Context::new().tagged(Arc::from("a")).with_event_bus(&bus);
    let a_counter = Arc::clone(&a_hits);
    let _a = owner_a.on::<Ping, _>(move |_| {
        let counter = Arc::clone(&a_counter);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Value::new(()))
        })
    });

    let owner_b = Context::new().tagged(Arc::from("b")).with_event_bus(&bus);
    let b_counter = Arc::clone(&b_hits);
    let _b = owner_b.on::<Ping, _>(move |_| {
        let counter = Arc::clone(&b_counter);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Value::new(()))
        })
    });

    let target = Context::new()
        .with_event_bus(&bus)
        .tagged(Arc::from("a"))
        .with_filter(|owner| owner.tag().map(|t| t.as_ref()) == Some("a"));
    bus.emit_scoped(&target, Arc::new(Ping));
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    assert_eq!(
        global.load(Ordering::SeqCst),
        1,
        "global listeners always run"
    );
    assert_eq!(
        a_hits.load(Ordering::SeqCst),
        1,
        "owned listener on target runs"
    );
    assert_eq!(
        b_hits.load(Ordering::SeqCst),
        0,
        "foreign owned listener skipped"
    );
}

#[test]
fn duplicate_provide_and_type_mismatch_are_errors() {
    let ctx = Context::new();
    let _first = ctx.provide::<Counter>(Arc::new(Counter)).unwrap();
    assert!(matches!(
        ctx.provide::<Counter>(Arc::new(Counter)),
        Err(cordis_rslite::ContextError::Duplicate(_))
    ));
}

#[test]
fn require_type_mismatch_is_error() {
    let ctx = Context::new();
    let _other = ctx.provide_any("counter", Value::new("a string")).unwrap();
    assert!(matches!(
        ctx.require::<Counter>(),
        Err(cordis_rslite::ContextError::TypeMismatch { .. })
    ));
}

#[tokio::test]
async fn fiber_dispose_runs_effects_lifo() {
    let fiber = Arc::new(Fiber::new());
    let order = Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
    let o1 = Arc::clone(&order);
    fiber
        .adopt_effect(EffectHandle::new(move || o1.lock().unwrap().push(1)))
        .unwrap();
    let o2 = Arc::clone(&order);
    fiber
        .adopt_effect(EffectHandle::new(move || o2.lock().unwrap().push(2)))
        .unwrap();

    fiber.dispose().await;
    assert_eq!(
        *order.lock().unwrap(),
        vec![2, 1],
        "fiber teardown must run effects in LIFO order"
    );
}

#[tokio::test]
async fn emit_limit_drops_overflow() {
    let bus = EventBus::with_emit_limit(1);
    let _slow = bus.on::<Ping, _>(|_| {
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(Value::new(()))
        })
    });
    bus.emit(Arc::new(Ping));
    bus.emit(Arc::new(Ping));
    bus.emit(Arc::new(Ping));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        bus.dropped_emits() >= 2,
        "emits beyond the concurrency limit must be dropped, not queued"
    );
}
