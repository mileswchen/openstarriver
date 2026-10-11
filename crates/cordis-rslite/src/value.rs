use std::any::{type_name, Any};
use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum ValueCastError {
    #[error("expected value of type `{expected}`, found `{actual}`")]
    TypeMismatch {
        expected: &'static str,
        actual: &'static str,
    },
}

#[derive(Clone)]
pub struct Value {
    inner: Arc<dyn Any + Send + Sync>,
    type_name: &'static str,
}

impl Value {
    pub fn new<T>(value: T) -> Self
    where
        T: Any + Send + Sync,
    {
        Self {
            inner: Arc::new(value),
            type_name: type_name::<T>(),
        }
    }

    pub fn from_arc<T>(value: Arc<T>) -> Self
    where
        T: Any + Send + Sync,
    {
        Self {
            inner: value,
            type_name: type_name::<T>(),
        }
    }

    pub fn downcast<T>(&self) -> Result<Arc<T>, ValueCastError>
    where
        T: Any + Send + Sync,
    {
        self.inner
            .clone()
            .downcast::<T>()
            .map_err(|_| ValueCastError::TypeMismatch {
                expected: type_name::<T>(),
                actual: self.type_name,
            })
    }

    pub fn try_downcast<T>(&self) -> Result<Arc<T>, crate::events::EventError>
    where
        T: Any + Send + Sync,
    {
        Ok(self.downcast::<T>()?)
    }

    pub fn type_name(&self) -> &'static str {
        self.type_name
    }

    pub fn is<T: Any>(&self) -> bool {
        self.inner.is::<T>()
    }

    /// Returns a reference to the underlying type-erased value.
    ///
    /// This is useful when callers only need a read-only `Any` reference,
    /// for example to inspect the concrete type without cloning the `Arc`.
    pub fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self.inner.as_ref()
    }

    /// Returns `true` when both values point to the same underlying `Arc`.
    ///
    /// This is a pointer comparison, not a value comparison. Two separately
    /// allocated values that are otherwise equal will compare as `false`.
    pub fn ptr_eq(&self, other: &Value) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Default for Value {
    fn default() -> Self {
        Self::new(())
    }
}

impl Debug for Value {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Value")
            .field("type_name", &self.type_name)
            .finish_non_exhaustive()
    }
}

/// Type-erased plugin/service configuration.
///
/// Internally this is the same runtime value as [`Value`]; the alias exists
/// to make the configuration flow explicit at the API boundary while keeping
/// typed access through [`Value::downcast`].
pub type Config = Value;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_any_exposes_underlying_any() {
        let value = Value::new(42u64);
        assert!(value.as_any().is::<u64>());
        assert!(!value.as_any().is::<String>());
    }

    #[test]
    fn ptr_eq_compares_arc_identity() {
        let shared = Arc::new(42u64);
        let a = Value::from_arc(Arc::clone(&shared));
        let b = Value::from_arc(Arc::clone(&shared));
        let c = Value::new(42u64);

        assert!(a.ptr_eq(&b));
        assert!(!a.ptr_eq(&c));
    }

    #[test]
    fn value_cast_error_reports_both_types() {
        let value = Value::new(42u64);
        let error = value.downcast::<String>().unwrap_err();
        let message = error.to_string();
        assert!(message.contains("u64"));
        assert!(message.contains("String"));
    }

    #[test]
    fn try_downcast_maps_to_event_cast_error() {
        let value = Value::new(42u64);
        let error = value.try_downcast::<String>().unwrap_err();
        assert!(matches!(error, crate::events::EventError::Cast(_)));
    }
}
