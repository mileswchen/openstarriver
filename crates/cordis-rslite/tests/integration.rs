use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use cordis_rslite::{
    plugin_sync_with_config, Config, Context, EffectHandle, Event, EventBus, Fiber, FiberState,
    Inject, Plugin, PluginRegistry, Service, TypedService, Value, WaterfallEvent,
};

struct Greeting(Arc<str>);
impl TypedService for Greeting {
    const NAME: &'static str = "greeting";
}

#[derive(Clone)]
struct Ping(u64);
impl Event for Ping {
    const NAME: &'static str = "demo/ping";
}
impl WaterfallEvent for Ping {
    type Output = u64;
}

struct DemoPlugin;

impl Plugin for DemoPlugin {
    fn name(&self) -> &'static str {
        "demo"
    }

    fn provides(&self) -> &[&'static str] {
        &["greeting"]
    }

    fn apply(
        &self,
        ctx: &Context,
        bus: &EventBus,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
        let ctx = ctx.clone();
        let bus = bus.clone();
        Box::pin(async move {
            Ok(vec![
                ctx.provide::<Greeting>(Arc::new(Greeting(Arc::from("integration"))))
                    .map_err(anyhow::Error::from)?,
                bus.on::<Ping, _>(|e| {
                    let n = e.0;
                    Box::pin(async move { Ok(Value::new(n * 2)) })
                }),
            ])
        })
    }
}

#[tokio::test]
async fn plugin_boot_and_event_roundtrip() {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    registry.register(DemoPlugin);
    let handles = registry.boot(&ctx, &bus).await.unwrap();

    assert_eq!(ctx.require::<Greeting>().unwrap().0.as_ref(), "integration");

    let v = bus.serial(Arc::new(Ping(21))).await.unwrap().unwrap();
    assert_eq!(*v.downcast::<u64>().unwrap(), 42);

    for handle in &handles {
        handle.shutdown().await;
    }
    assert!(!ctx.has("greeting"));
    assert!(bus.serial(Arc::new(Ping(1))).await.unwrap().is_none());
}

#[tokio::test]
async fn fiber_adopts_effects_and_disposes() {
    let fiber = Fiber::new();
    let ctx = Context::new();
    let handle = ctx
        .provide::<Greeting>(Arc::new(Greeting(Arc::from("x"))))
        .unwrap();
    fiber.adopt(handle.into_cleanup().unwrap()).unwrap();
    assert!(ctx.has("greeting"));

    fiber.dispose().await;
    assert_eq!(fiber.state(), FiberState::Disposed);
    assert!(!ctx.has("greeting"));
}

#[tokio::test]
async fn waterfall_pipeline_roundtrip() {
    let bus = EventBus::new();
    let _h = bus.on_waterfall::<Ping, _>(|_event, next| {
        Box::pin(async move {
            let value = next().await?;
            Ok(value + 1)
        })
    });
    let out = bus
        .waterfall(Arc::new(Ping(0)), || Box::pin(async { Ok(1u64) }))
        .await
        .unwrap();
    assert_eq!(out, 2);
}

struct ProviderSvc(Arc<str>);
impl TypedService for ProviderSvc {
    const NAME: &'static str = "provider-svc";
}

struct Provider;
impl Plugin for Provider {
    fn name(&self) -> &'static str {
        "provider"
    }
    fn provides(&self) -> &[&'static str] {
        &["provider-svc"]
    }
    fn apply(
        &self,
        ctx: &Context,
        _bus: &EventBus,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
        let ctx = ctx.clone();
        Box::pin(async move {
            ctx.provide::<ProviderSvc>(Arc::new(ProviderSvc(Arc::from("p"))))
                .map(|handle| vec![handle])
                .map_err(anyhow::Error::from)
        })
    }
}

struct Consumer;
impl Plugin for Consumer {
    fn name(&self) -> &'static str {
        "consumer"
    }
    fn inject(&self) -> Inject {
        Inject::new(["provider-svc"])
    }
    fn apply(
        &self,
        _ctx: &Context,
        _bus: &EventBus,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<Vec<EffectHandle>>> {
        Box::pin(async { Ok(vec![]) })
    }
}

#[tokio::test]
async fn di_chain_boots_in_any_order() {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    registry.register(Consumer);
    registry.register(Provider);
    let handles = registry.boot(&ctx, &bus).await.unwrap();
    assert_eq!(handles.len(), 2);
    assert!(handles.iter().all(|h| h.state() == FiberState::Active));
    assert_eq!(ctx.require::<ProviderSvc>().unwrap().0.as_ref(), "p");
}

struct MixedEffectPlugin {
    cleaned: Arc<AtomicBool>,
}

impl Plugin for MixedEffectPlugin {
    fn name(&self) -> &'static str {
        "mixed-effect"
    }
    fn apply(
        &self,
        ctx: &Context,
        bus: &EventBus,
    ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let ctx = ctx.clone();
        let bus = bus.clone();
        let cleaned = Arc::clone(&self.cleaned);
        Box::pin(async move {
            let service = ctx
                .provide::<Greeting>(Arc::new(Greeting(Arc::from("mixed"))))
                .map_err(anyhow::Error::from)?;
            let listener = bus.on::<Ping, _>(|_| Box::pin(async { Ok(Value::new(1u64)) }));
            let async_effect = EffectHandle::new_async(move || {
                let cleaned = Arc::clone(&cleaned);
                Box::pin(async move {
                    cleaned.store(true, Ordering::SeqCst);
                })
            });
            Ok(vec![service, listener, async_effect])
        })
    }
}

#[tokio::test]
async fn mixed_effects_clean_up_on_shutdown() {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    let cleaned = Arc::new(AtomicBool::new(false));
    registry.register(MixedEffectPlugin {
        cleaned: Arc::clone(&cleaned),
    });
    let handles = registry.boot(&ctx, &bus).await.unwrap();

    assert_eq!(ctx.require::<Greeting>().unwrap().0.as_ref(), "mixed");
    assert!(bus.serial(Arc::new(Ping(1))).await.unwrap().is_some());
    assert!(!cleaned.load(Ordering::SeqCst));

    handles[0].shutdown().await;
    assert!(!ctx.has("greeting"));
    assert!(bus.serial(Arc::new(Ping(1))).await.unwrap().is_none());
    assert!(cleaned.load(Ordering::SeqCst));
}

struct AllEventsPlugin;

impl Plugin for AllEventsPlugin {
    fn name(&self) -> &'static str {
        "all-events"
    }

    fn apply(
        &self,
        _ctx: &Context,
        bus: &EventBus,
    ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let bus = bus.clone();
        Box::pin(async move {
            let async_listener = bus.on::<Ping, _>(|event| {
                let n = event.0;
                Box::pin(async move { Ok(Value::new(n * 3)) })
            });
            let sync = bus.on_sync::<Ping, _>(|_| Ok(Value::new(7u64)));
            let waterfall = bus.on_waterfall::<Ping, _>(|_event, next| {
                Box::pin(async move {
                    let value = next().await?;
                    Ok(value + 5)
                })
            });
            Ok(vec![async_listener, sync, waterfall])
        })
    }
}

#[tokio::test]
async fn plugin_event_listeners_dispatch_and_dispose_together() {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    registry.register(AllEventsPlugin);
    let handles = registry.boot(&ctx, &bus).await.unwrap();

    assert_eq!(bus.listener_count(Ping::NAME), 3);

    let sync_results = bus.emit_sync(Arc::new(Ping(1)));
    assert_eq!(
        *sync_results[0].as_ref().unwrap().downcast::<u64>().unwrap(),
        7
    );

    let async_value = bus.serial(Arc::new(Ping(2))).await.unwrap().unwrap();
    assert_eq!(*async_value.downcast::<u64>().unwrap(), 6);

    let waterfall_out = bus
        .waterfall(Arc::new(Ping(0)), || Box::pin(async { Ok(1u64) }))
        .await
        .unwrap();
    assert_eq!(waterfall_out, 6);

    handles[0].shutdown().await;
    assert_eq!(bus.listener_count(Ping::NAME), 0);
    assert!(bus.emit_sync(Arc::new(Ping(1))).is_empty());
    assert!(bus.serial(Arc::new(Ping(1))).await.unwrap().is_none());

    let waterfall_out = bus
        .waterfall(Arc::new(Ping(0)), || Box::pin(async { Ok(1u64) }))
        .await
        .unwrap();
    assert_eq!(waterfall_out, 1);
}

struct OncePlugin(Arc<AtomicUsize>);

impl Plugin for OncePlugin {
    fn name(&self) -> &'static str {
        "once-plugin"
    }

    fn apply(
        &self,
        _ctx: &Context,
        bus: &EventBus,
    ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let bus = bus.clone();
        let calls = Arc::clone(&self.0);
        Box::pin(async move {
            let handle = bus.once::<Ping, _>(move |_| {
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(Value::new(1u64))
                })
            });
            Ok(vec![handle])
        })
    }
}

#[tokio::test]
async fn once_listener_consumed_and_cleaned_up_after_plugin_boot() {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    let calls = Arc::new(AtomicUsize::new(0));
    registry.register(OncePlugin(Arc::clone(&calls)));

    let handles = registry.boot(&ctx, &bus).await.unwrap();
    assert_eq!(bus.listener_count(Ping::NAME), 1);

    assert!(bus.serial(Arc::new(Ping(0))).await.unwrap().is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(bus.listener_count(Ping::NAME), 0);

    assert!(bus.serial(Arc::new(Ping(0))).await.unwrap().is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    handles[0].shutdown().await;
    assert_eq!(bus.listener_count(Ping::NAME), 0);
}

struct InternalGetPlugin;

impl Plugin for InternalGetPlugin {
    fn name(&self) -> &'static str {
        "internal-get"
    }

    fn apply(
        &self,
        _ctx: &Context,
        bus: &EventBus,
    ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let bus = bus.clone();
        Box::pin(async move {
            let handle = bus.dynamic().on_waterfall("internal/get", |event, next| {
                Box::pin(async move {
                    let name = event.args()[1].downcast::<String>().unwrap();
                    if name.as_str() == "greeting" {
                        Ok(Some(Value::new(Greeting(Arc::from("virtual")))))
                    } else {
                        next().await
                    }
                })
            });
            Ok(vec![handle])
        })
    }
}

#[tokio::test]
async fn internal_get_interception_via_plugin_virtualizes_service() {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    registry.register(InternalGetPlugin);
    let handles = registry.boot(&ctx, &bus).await.unwrap();
    assert!(handles.iter().all(|h| h.state() == FiberState::Active));

    let ctx_with_bus = ctx.with_event_bus(&bus);
    let got = ctx_with_bus
        .require_named_async::<Greeting>("greeting")
        .await
        .unwrap();
    assert_eq!(got.0.as_ref(), "virtual");

    handles[0].shutdown().await;
}

struct InternalSetVetoPlugin;

impl Plugin for InternalSetVetoPlugin {
    fn name(&self) -> &'static str {
        "internal-set-veto"
    }

    fn apply(
        &self,
        _ctx: &Context,
        bus: &EventBus,
    ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let bus = bus.clone();
        Box::pin(async move {
            let handle = bus.dynamic().on_waterfall("internal/set", |_event, _next| {
                Box::pin(async { Ok(Some(Value::new(false))) })
            });
            Ok(vec![handle])
        })
    }
}

#[tokio::test]
async fn internal_set_veto_via_plugin_blocks_provide() {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    registry.register(InternalSetVetoPlugin);
    let handles = registry.boot(&ctx, &bus).await.unwrap();

    let ctx_with_bus = ctx.with_event_bus(&bus);
    let _h = ctx_with_bus
        .provide_named_async::<Greeting>(
            "greeting".to_owned(),
            Arc::new(Greeting(Arc::from("nope"))),
        )
        .await
        .unwrap();
    assert!(!ctx.has("greeting"));

    handles[0].shutdown().await;
}

struct ReplacementProvider;

impl Plugin for ReplacementProvider {
    fn name(&self) -> &'static str {
        "replacement-provider"
    }

    fn provides(&self) -> &[&'static str] {
        &["provider-svc"]
    }

    fn apply(
        &self,
        ctx: &Context,
        _bus: &EventBus,
    ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let ctx = ctx.clone();
        Box::pin(async move {
            ctx.provide::<ProviderSvc>(Arc::new(ProviderSvc(Arc::from("q"))))
                .map(|handle| vec![handle])
                .map_err(anyhow::Error::from)
        })
    }
}

#[tokio::test]
async fn context_dynamically_adds_and_removes_a_plugin() {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    let handles = registry.boot(&ctx, &bus).await.unwrap();
    assert!(handles.is_empty());

    let handle = ctx.plugin(Provider).await.unwrap();
    assert_eq!(handle.state(), FiberState::Active);
    assert_eq!(ctx.require::<ProviderSvc>().unwrap().0.as_ref(), "p");

    assert!(ctx.remove_plugin(handle.key()).await);
    assert!(!ctx.has("provider-svc"));
    assert!(!ctx.remove_plugin(handle.key()).await);
}

#[tokio::test]
async fn context_replaces_a_plugin_by_key() {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    let _ = registry.boot(&ctx, &bus).await.unwrap();

    let old = ctx.plugin(Provider).await.unwrap();
    assert_eq!(ctx.require::<ProviderSvc>().unwrap().0.as_ref(), "p");

    let new = ctx
        .replace_plugin(old.key(), ReplacementProvider)
        .await
        .unwrap();
    assert_ne!(old.key(), new.key());
    assert_eq!(ctx.require::<ProviderSvc>().unwrap().0.as_ref(), "q");
}

struct InvalidConfigPlugin;

impl Plugin for InvalidConfigPlugin {
    fn name(&self) -> &'static str {
        "invalid-config"
    }

    fn validate_config(&self, _config: Config) -> anyhow::Result<Config> {
        anyhow::bail!("invalid configuration")
    }

    fn apply_with_config(
        &self,
        _ctx: &Context,
        _bus: &EventBus,
        _config: Config,
    ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        Box::pin(async { Ok(vec![]) })
    }
}

#[tokio::test]
async fn invalid_config_boot_fails_and_cleans_partial_activation() {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    registry.register(DemoPlugin);
    registry.register_with_config(InvalidConfigPlugin, Config::new(()));

    let error = match registry.boot(&ctx, &bus).await {
        Ok(_) => panic!("expected boot to fail"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("invalid configuration"));
    assert!(!ctx.has("greeting"));
}

struct InternalGetWrongTypePlugin;

impl Plugin for InternalGetWrongTypePlugin {
    fn name(&self) -> &'static str {
        "internal-get-wrong-type"
    }

    fn apply(
        &self,
        _ctx: &Context,
        bus: &EventBus,
    ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<EffectHandle>>> {
        let bus = bus.clone();
        Box::pin(async move {
            let handle = bus.dynamic().on_waterfall("internal/get", |event, next| {
                Box::pin(async move {
                    let name = event.args()[1].downcast::<String>().unwrap();
                    if name.as_str() == "greeting" {
                        Ok(Some(Value::new(42u64)))
                    } else {
                        next().await
                    }
                })
            });
            Ok(vec![handle])
        })
    }
}

#[tokio::test]
async fn internal_get_type_mismatch_is_reported() {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    registry.register(InternalGetWrongTypePlugin);
    let handles = registry.boot(&ctx, &bus).await.unwrap();
    assert!(handles.iter().all(|h| h.state() == FiberState::Active));

    let ctx_with_bus = ctx.with_event_bus(&bus);
    assert!(ctx_with_bus
        .require_named_async::<Greeting>("greeting")
        .await
        .is_err());

    handles[0].shutdown().await;
}

#[tokio::test]
async fn plugin_update_after_removal_fails() {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    registry.register_with_config(
        plugin_sync_with_config::<u64, _>(
            "removable-integration",
            Inject::none(),
            |ctx, _bus, config| {
                ctx.provide_any("removable-integration-count", Value::new(*config))
                    .map(|handle| vec![handle])
                    .map_err(anyhow::Error::from)
            },
        ),
        Config::new(1u64),
    );
    let handles = registry.boot(&ctx, &bus).await.unwrap();
    assert_eq!(
        *ctx.require_any("removable-integration-count")
            .unwrap()
            .downcast::<u64>()
            .unwrap(),
        1
    );

    assert!(ctx.remove_plugin(handles[0].key()).await);
    let error = match handles[0].update(Config::new(2u64)).await {
        Ok(_) => panic!("expected update after removal to fail"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("has been removed"));
}
