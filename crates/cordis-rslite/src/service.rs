/// A named service that plugins can provide and consume through a Context.
///
/// Object-safe by design: `Arc<dyn Service>` is a valid service value and can
/// be provided through [`Context::provide_dyn`]. The compile-time name used by
/// the zero-argument typed accessors lives on [`TypedService`].
pub trait Service: Send + Sync + 'static {
    /// The runtime service name of this instance.
    fn name(&self) -> &'static str;

    /// Whether dependents may use this service immediately; when false, `dependency_epoch` treats it as unavailable.
    fn is_available(&self) -> bool {
        true
    }
}

/// A [`Service`] whose name is fixed at compile time.
///
/// Every statically-typed service implements this trait; the blanket impl
/// below bridges it into [`Service`]. The zero-argument `provide::<T>()` /
/// `require::<T>()` accessors key off [`TypedService::NAME`], so their name
/// resolution stays purely compile-time. Runtime-named services (including
/// `dyn Service` values) implement [`Service`] directly instead.
pub trait TypedService: Service {
    const NAME: &'static str;

    /// Redeclared so implementors can override availability while only
    /// implementing `TypedService`; forwarded through the blanket `Service`
    /// impl.
    fn is_available(&self) -> bool {
        true
    }
}

impl<T: TypedService> Service for T {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn is_available(&self) -> bool {
        TypedService::is_available(self)
    }
}
