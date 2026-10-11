//! The cordis-rslite plugin framework: composition only, no product logic.

#![deny(unused_must_use)]

pub type BoxFuture<T> = futures::future::BoxFuture<'static, T>;

/// Declares a strongly-typed event struct together with its `Event` name.
///
/// The macro reduces the boilerplate of hand-writing a struct and an
/// `Event` implementation, while keeping the event fully typed:
///
/// ```ignore
/// cordis_rslite::cordis_event!(StatsReport, "stats/report", {
///     name: String,
///     count: u64,
/// });
/// ```
///
/// This expands to a `pub struct StatsReport` with public fields and an
/// `Event` implementation whose `NAME` is `"stats/report"`. An optional
/// visibility qualifier can be placed before the struct name:
///
/// ```ignore
/// cordis_rslite::cordis_event!(pub(crate) InternalEvent, "internal/event", {});
/// ```
#[macro_export]
macro_rules! cordis_event {
    ($name:ident, $event_name:literal, { $($field:ident : $ty:ty),* $(,)? }) => {
        pub struct $name {
            $(pub $field: $ty,)*
        }

        impl $crate::Event for $name {
            const NAME: &'static str = $event_name;
        }
    };
    ($vis:vis $name:ident, $event_name:literal, { $($field:ident : $ty:ty),* $(,)? }) => {
        $vis struct $name {
            $(pub $field: $ty,)*
        }

        impl $crate::Event for $name {
            const NAME: &'static str = $event_name;
        }
    };
}

/// Declare a static event catalog as `(&'static str, &'static str)` pairs.
///
/// ```ignore
/// cordis_rslite::event_catalog!(EVENT_CATALOG, {
///     "stats/report" => StatsReport,
///     "demo/transform" => DemoTransform,
/// });
/// ```
#[macro_export]
macro_rules! event_catalog {
    ($name:ident, { $($event_name:literal => $event_ty:ty),* $(,)? }) => {
        pub fn $name() -> Vec<(&'static str, &'static str)> {
            vec![
                $(($event_name, std::any::type_name::<$event_ty>())),*
            ]
        }
    };
}

pub mod context;
pub mod effect;
pub mod events;
pub mod fiber;
pub mod plugin;
pub mod service;
pub mod value;

pub use context::{
    is_public_seam, Context, ContextError, ContextFilter, Isolation, KERNEL_PREFIX, SEAM_PREFIX,
};
pub use effect::EffectHandle;
pub use events::{
    DynamicEvent, DynamicEvents, Event, EventBus, EventError, EventOptions, Next, WaterfallEvent,
};
pub use fiber::{Fiber, FiberState};
pub use plugin::{
    plugin_async, plugin_async_with_config, plugin_sync, plugin_sync_with_config, service_async,
    service_async_with_config, service_sync, service_sync_with_config, Dependency, Inject, Plugin,
    PluginHandle, PluginKey, PluginRegistry,
};
pub use service::{Service, TypedService};
pub use value::{Config, Value, ValueCastError};
