//! Method registry: the authoritative dispatch point for `invoke`.
//!
//! Each method is one [`MethodSpec`]: parameter schema (validation),
//! permissions (caller filtering), whether it streams, and a handler that
//! forwards to a typed host method. The registry is the single source of
//! truth; transports (IPC/HTTP) only deliver the wire format and make no
//! business decisions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::permission::CallerScope;
use crate::schema;
use crate::wire::{ApiError, EventEnvelope, Request, Response, PROTOCOL_VERSION};

/// Context for a single method call: host, parameters, caller, and the
/// streaming event callback.
pub struct MethodCall<'a, H> {
    pub host: &'a H,
    pub params: Value,
    pub caller: Option<&'a CallerScope>,
    /// Streaming methods use this to send back [`EventEnvelope`]s; `None`
    /// means the caller has not subscribed to events.
    pub emit: Option<Arc<dyn Fn(EventEnvelope) + Send + Sync>>,
}

/// Method handler: synchronous signature; decides internally whether to
/// spawn async work (e.g. executing a relay).
pub type MethodHandler<H> = Arc<dyn Fn(MethodCall<'_, H>) -> Result<Value, ApiError> + Send + Sync>;

/// A single method contract.
pub struct MethodSpec<H> {
    pub name: &'static str,
    pub params_schema: Value,
    pub result_schema: Value,
    /// Permissions the method requires; builtin callers are exempt, plugin
    /// frames are checked against the allowlist.
    pub permissions: &'static [&'static str],
    /// Streaming method: `invoke` returns `{"subscription": ...}` immediately;
    /// events flow through emit.
    pub streaming: bool,
    pub handler: MethodHandler<H>,
}

impl<H> Clone for MethodSpec<H> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            params_schema: self.params_schema.clone(),
            result_schema: self.result_schema.clone(),
            permissions: self.permissions,
            streaming: self.streaming,
            handler: Arc::clone(&self.handler),
        }
    }
}

impl<H> MethodSpec<H> {
    pub fn new(
        name: &'static str,
        params_schema: Value,
        result_schema: Value,
        permissions: &'static [&'static str],
        streaming: bool,
        handler: impl for<'a> Fn(MethodCall<'a, H>) -> Result<Value, ApiError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name,
            params_schema,
            result_schema,
            permissions,
            streaming,
            handler: Arc::new(handler),
        }
    }
}

/// Method catalog (ordered, exposed via `protocol.info`).
pub struct MethodRegistry<H> {
    ordered: Vec<MethodSpec<H>>,
    by_name: HashMap<&'static str, usize>,
    idempotency: Arc<IdempotencyStore>,
}

impl<H> Clone for MethodRegistry<H> {
    fn clone(&self) -> Self {
        Self {
            ordered: self.ordered.clone(),
            by_name: self.by_name.clone(),
            idempotency: Arc::clone(&self.idempotency),
        }
    }
}

impl<H> MethodRegistry<H> {
    /// Builds the catalog and auto-synthesizes `protocol.info`
    /// (handshake/discovery entry point).
    pub fn new(mut specs: Vec<MethodSpec<H>>) -> Self {
        let catalog_json = specs
            .iter()
            .map(|spec| {
                json!({
                    "name": spec.name,
                    "permissions": spec.permissions,
                    "streaming": spec.streaming,
                })
            })
            .collect::<Vec<_>>();
        specs.push(MethodSpec::new(
            "protocol.info",
            schema::object(&[], &[]),
            schema::any_schema(),
            &[],
            false,
            move |_call| {
                Ok(json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "methods": catalog_json,
                }))
            },
        ));
        let by_name = specs
            .iter()
            .enumerate()
            .map(|(index, spec)| (spec.name, index))
            .collect::<HashMap<_, _>>();
        Self {
            ordered: specs,
            by_name,
            idempotency: Arc::new(IdempotencyStore::default()),
        }
    }

    pub fn get(&self, name: &str) -> Option<&MethodSpec<H>> {
        self.by_name
            .get(name)
            .and_then(|index| self.ordered.get(*index))
    }

    /// Method catalog (name + permissions + streaming flag), for
    /// `protocol.info` and schema export.
    pub fn catalog(&self) -> Vec<Value> {
        self.ordered
            .iter()
            .map(|spec| {
                json!({
                    "name": spec.name,
                    "permissions": spec.permissions,
                    "streaming": spec.streaming,
                })
            })
            .collect()
    }

    /// Full schema (version + method catalog + per-method parameter/result
    /// schemas), used to export `protocol.schema.json` and generate the TS
    /// client.
    pub fn to_schema(&self) -> Value {
        let methods = self
            .ordered
            .iter()
            .map(|spec| {
                json!({
                    "name": spec.name,
                    "params": spec.params_schema,
                    "result": spec.result_schema,
                    "permissions": spec.permissions,
                    "streaming": spec.streaming,
                })
            })
            .collect::<Vec<_>>();
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "methods": methods,
        })
    }

    /// Dispatches a request: version check → method exists → parameter
    /// validation → permission check → handler.
    pub fn dispatch(
        &self,
        host: &H,
        request: &Request,
        caller: Option<&CallerScope>,
        emit: Option<Arc<dyn Fn(EventEnvelope) + Send + Sync>>,
    ) -> Result<Response, ApiError> {
        if request.v != PROTOCOL_VERSION {
            return Err(ApiError::protocol_version(request.v));
        }
        // Idempotency: same id already completed → reuse the cached
        // response directly; in flight → don't retrigger.
        if let Some(hit) = self.idempotency.get(&request.id) {
            return match hit {
                IdempotencyHit::Done(response) => Ok(*response),
                IdempotencyHit::InFlight => Err(ApiError::new(
                    "IDEMPOTENT_IN_FLIGHT",
                    format!(
                        "request `{}` is already being processed; retry with the same id to reuse its result",
                        request.id
                    ),
                )),
            };
        }
        let spec = self
            .by_name
            .get(request.method.as_str())
            .and_then(|index| self.ordered.get(*index))
            .ok_or_else(|| ApiError::method_not_found(&request.method))?;
        // `params` is `#[serde(default)]` — an omitted field deserializes to
        // JSON null, which every object schema would reject. An absent
        // params means "no parameters" (empty object), matching the
        // wire-level default.
        let effective_params = if request.params.is_null() {
            serde_json::json!({})
        } else {
            request.params.clone()
        };
        schema::validate(&spec.params_schema, &effective_params)?;
        if let Some(caller) = caller {
            if !caller.allows(spec.permissions) {
                return Err(ApiError::forbidden(format!(
                    "caller `{}` lacks permission for `{}` (requires {:?})",
                    caller.scope, spec.name, spec.permissions
                )));
            }
        }
        let call = MethodCall {
            host,
            params: effective_params,
            caller,
            emit,
        };
        self.idempotency.mark_in_flight(&request.id);
        // BUG-60: a handler panic must not leak the InFlight entry (the id
        // would be permanently un-retryable as IDEMPOTENT_IN_FLIGHT) — catch
        // the unwind, abort the entry, surface a retryable internal error.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (spec.handler)(call)
        }));
        let outcome = match outcome {
            Ok(result) => result,
            Err(panic) => {
                self.idempotency.abort(&request.id);
                let message = if let Some(text) = panic.downcast_ref::<&str>() {
                    text.to_string()
                } else if let Some(text) = panic.downcast_ref::<String>() {
                    text.clone()
                } else {
                    "handler panicked".into()
                };
                return Err(ApiError::new("HANDLER_PANIC", message));
            }
        };
        match outcome {
            Ok(value) => {
                let response = Response::ok(request, value);
                self.idempotency.complete(&request.id, response.clone());
                Ok(response)
            }
            Err(error) => {
                // Failures are not cached: transient errors (SESSION_BUSY
                // etc.) may be retried with the same id.
                self.idempotency.abort(&request.id);
                Err(error)
            }
        }
    }
}

/// Idempotency hit result.
pub enum IdempotencyHit {
    /// This id has completed; reuse the cached response directly.
    Done(Box<Response>),
    /// This id is currently being processed (triggered by another request,
    /// not yet finished).
    InFlight,
}

/// Request idempotency store: id → most recent successful response
/// (TTL + capacity cap).
///
/// Semantics (protocol §3.2): when a client retries with the same `id`, the
/// command is not retriggered; only successful responses are cached —
/// failures may be retried with the same id.
const IDEMPOTENCY_TTL: Duration = Duration::from_secs(15 * 60);
const IDEMPOTENCY_MAX: usize = 8192;
const IDEMPOTENCY_STATS_EVERY: u64 = 256;

#[derive(Default)]
pub struct IdempotencyStore {
    inner: Mutex<HashMap<String, Entry>>,
    ops: AtomicU64,
}

struct Entry {
    done: bool,
    response: Option<Response>,
    inserted: Instant,
}

impl IdempotencyStore {
    pub fn get(&self, id: &str) -> Option<IdempotencyHit> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.maybe_prune(&mut inner);
        let entry = inner.get(id)?;
        if entry.done {
            entry
                .response
                .clone()
                .map(|response| IdempotencyHit::Done(Box::new(response)))
        } else {
            Some(IdempotencyHit::InFlight)
        }
    }

    pub fn mark_in_flight(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.maybe_prune(&mut inner);
        inner.insert(
            id.to_string(),
            Entry {
                done: false,
                response: None,
                inserted: Instant::now(),
            },
        );
    }

    pub fn complete(&self, id: &str, response: Response) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = inner.get_mut(id) {
            entry.done = true;
            entry.response = Some(response);
        }
    }

    pub fn abort(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.remove(id);
    }

    fn maybe_prune(&self, inner: &mut HashMap<String, Entry>) {
        let ops = self.ops.fetch_add(1, Ordering::Relaxed);
        if !ops.is_multiple_of(IDEMPOTENCY_STATS_EVERY) && inner.len() < IDEMPOTENCY_MAX {
            return;
        }
        let now = Instant::now();
        inner
            .retain(|_, entry| !entry.done || now.duration_since(entry.inserted) < IDEMPOTENCY_TTL);
        if inner.len() >= IDEMPOTENCY_MAX {
            // Extreme case: in-flight requests exceed capacity; drop
            // completed entries to make room.
            inner.retain(|_, entry| !entry.done);
            if inner.len() >= IDEMPOTENCY_MAX {
                tracing::warn!(count = inner.len(), "idempotency store overflow; clearing");
                inner.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::PERM_SESSION_EXECUTE;
    use crate::schema::{integer_schema, object, string_schema};

    struct FakeHost {
        executed: std::sync::atomic::AtomicUsize,
    }

    fn fake_registry() -> MethodRegistry<FakeHost> {
        MethodRegistry::new(vec![
            MethodSpec::new(
                "echo.echo",
                object(&[("text", string_schema())], &["text"]),
                object(&[("text", string_schema())], &["text"]),
                &[],
                false,
                |call: MethodCall<'_, FakeHost>| {
                    call.host
                        .executed
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(json!({ "text": call.params["text"].as_str().unwrap() }))
                },
            ),
            MethodSpec::new(
                "session.execute",
                object(
                    &[("task", string_schema()), ("timeout", integer_schema())],
                    &["task"],
                ),
                object(&[("subscription", string_schema())], &["subscription"]),
                &[PERM_SESSION_EXECUTE],
                true,
                |call| {
                    if let Some(emit) = call.emit {
                        emit(EventEnvelope::new(
                            "sub_1",
                            1,
                            "turnStart",
                            json!({ "task": call.params["task"] }),
                        ));
                    }
                    Ok(json!({ "subscription": "sub_1" }))
                },
            ),
        ])
    }

    fn request(method: &str, params: Value) -> Request {
        Request {
            v: PROTOCOL_VERSION,
            id: "req_1".into(),
            method: method.into(),
            params,
        }
    }

    fn request_with_id(id: &str, method: &str, params: Value) -> Request {
        Request {
            v: PROTOCOL_VERSION,
            id: id.into(),
            method: method.into(),
            params,
        }
    }

    #[test]
    fn dispatch_validates_and_runs_handler() {
        let registry = fake_registry();
        let host = FakeHost {
            executed: std::sync::atomic::AtomicUsize::new(0),
        };
        let response = registry
            .dispatch(
                &host,
                &request("echo.echo", json!({ "text": "hi" })),
                None,
                None,
            )
            .unwrap();
        assert!(response.ok);
        assert_eq!(response.result.unwrap()["text"], "hi");
    }

    #[test]
    fn dispatch_rejects_unknown_method() {
        let registry = fake_registry();
        let host = FakeHost {
            executed: std::sync::atomic::AtomicUsize::new(0),
        };
        let error = registry
            .dispatch(&host, &request("nope.nope", json!({})), None, None)
            .unwrap_err();
        assert_eq!(error.code, "METHOD_NOT_FOUND");
    }

    #[test]
    fn dispatch_rejects_bad_params() {
        let registry = fake_registry();
        let host = FakeHost {
            executed: std::sync::atomic::AtomicUsize::new(0),
        };
        let error = registry
            .dispatch(&host, &request("echo.echo", json!({})), None, None)
            .unwrap_err();
        assert_eq!(error.code, "INVALID_PARAMS");
    }

    #[test]
    fn dispatch_rejects_wrong_protocol_version() {
        let registry = fake_registry();
        let host = FakeHost {
            executed: std::sync::atomic::AtomicUsize::new(0),
        };
        let mut req = request("echo.echo", json!({ "text": "hi" }));
        req.v = 99;
        let error = registry.dispatch(&host, &req, None, None).unwrap_err();
        assert_eq!(error.code, "PROTOCOL_VERSION");
    }

    #[test]
    fn dispatch_enforces_caller_permissions() {
        let registry = fake_registry();
        let host = FakeHost {
            executed: std::sync::atomic::AtomicUsize::new(0),
        };
        let plugin = CallerScope::plugin("demo", vec!["session:read".into()]);
        let error = registry
            .dispatch(
                &host,
                &request("session.execute", json!({ "task": "x" })),
                Some(&plugin),
                None,
            )
            .unwrap_err();
        assert_eq!(error.code, "FORBIDDEN");

        let allowed = CallerScope::plugin("demo", vec![PERM_SESSION_EXECUTE.into()]);
        let response = registry
            .dispatch(
                &host,
                &request("session.execute", json!({ "task": "x" })),
                Some(&allowed),
                None,
            )
            .unwrap();
        assert!(response.ok);
    }

    #[test]
    fn streaming_method_emits_events() {
        let registry = fake_registry();
        let host = FakeHost {
            executed: std::sync::atomic::AtomicUsize::new(0),
        };
        let emitted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let emit_clone = Arc::clone(&emitted);
        let emit: Arc<dyn Fn(EventEnvelope) + Send + Sync> = Arc::new(move |envelope| {
            emit_clone.lock().unwrap().push(envelope);
        });
        let response = registry
            .dispatch(
                &host,
                &request("session.execute", json!({ "task": "x" })),
                None,
                Some(emit),
            )
            .unwrap();
        assert_eq!(response.result.unwrap()["subscription"], "sub_1");
        let events = emitted.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].subscription, "sub_1");
        assert_eq!(events[0].event_type, "turnStart");
    }

    #[test]
    fn schema_export_lists_methods() {
        let registry = fake_registry();
        let schema = registry.to_schema();
        assert_eq!(schema["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(schema["methods"].as_array().unwrap().len(), 3);
        let execute = schema["methods"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "session.execute")
            .unwrap();
        assert_eq!(execute["streaming"], true);
        assert_eq!(execute["permissions"][0], "session:execute");
        assert!(schema["methods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "protocol.info"));
    }

    #[test]
    fn protocol_info_handshake_works() {
        let registry = fake_registry();
        let host = FakeHost {
            executed: std::sync::atomic::AtomicUsize::new(0),
        };
        let response = registry
            .dispatch(&host, &request("protocol.info", json!({})), None, None)
            .unwrap();
        let result = response.result.unwrap();
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        // protocol.info lists the catalog excluding itself.
        assert_eq!(result["methods"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn idempotent_retry_reuses_cached_response() {
        let registry = fake_registry();
        let host = FakeHost {
            executed: std::sync::atomic::AtomicUsize::new(0),
        };
        let req = request("echo.echo", json!({ "text": "hi" }));
        let first = registry.dispatch(&host, &req, None, None).unwrap();
        let second = registry.dispatch(&host, &req, None, None).unwrap();
        assert_eq!(first, second, "same id must return the cached response");
        assert_eq!(
            host.executed.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "same id must not retrigger the handler"
        );
    }

    #[test]
    fn in_flight_same_id_is_not_retriggered() {
        let registry = Arc::new(MethodRegistry::new(vec![MethodSpec::new(
            "echo.slow",
            object(&[], &[]),
            object(&[], &[]),
            &[],
            false,
            |_call| {
                std::thread::sleep(std::time::Duration::from_millis(120));
                Ok(json!({}))
            },
        )]));
        let host = FakeHost {
            executed: std::sync::atomic::AtomicUsize::new(0),
        };
        let req = request("echo.slow", json!({}));
        let registry2 = Arc::clone(&registry);
        let req2 = req.clone();
        let worker = std::thread::spawn(move || {
            let host = FakeHost {
                executed: std::sync::atomic::AtomicUsize::new(0),
            };
            registry2.dispatch(&host, &req2, None, None)
        });
        std::thread::sleep(std::time::Duration::from_millis(30));
        let in_flight = registry.dispatch(&host, &req, None, None).unwrap_err();
        assert_eq!(in_flight.code, "IDEMPOTENT_IN_FLIGHT");
        assert!(worker.join().unwrap().is_ok());
        // After completion, retrying with the same id reuses the cache
        // directly (handler not retriggered).
        assert!(registry.dispatch(&host, &req, None, None).unwrap().ok);
    }

    #[test]
    fn failed_request_is_not_cached() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        let registry = MethodRegistry::new(vec![MethodSpec::new(
            "echo.fail",
            object(&[], &[]),
            object(&[], &[]),
            &[],
            false,
            move |_call| {
                calls2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(ApiError::new("BOOM", "boom"))
            },
        )]);
        let host = FakeHost {
            executed: std::sync::atomic::AtomicUsize::new(0),
        };
        let req = request("echo.fail", json!({}));
        assert!(registry.dispatch(&host, &req, None, None).is_err());
        assert!(registry.dispatch(&host, &req, None, None).is_err());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "failed requests must not be cached"
        );
    }
}

// ── BUG-60 regression (round 5): a panicking handler must not leak the
// idempotency InFlight entry (same id retried forever as IN_FLIGHT) ──
#[cfg(test)]
mod bug60_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn panic_in_handler_does_not_permanently_lock_the_id() {
        struct PanicHost;
        let text_param =
            crate::schema::object(&[("text", crate::schema::string_schema())], &["text"]);
        let empty = crate::schema::object(&[], &[]);
        let registry = MethodRegistry::<PanicHost>::new(vec![MethodSpec::new(
            "boom.boom",
            text_param.clone(),
            empty,
            &[],
            false,
            |_call: MethodCall<'_, PanicHost>| -> Result<Value, ApiError> {
                panic!("handler exploded");
            },
        )]);
        // The panic must not escape dispatch (the HTTP server's spawn_blocking
        // turns it into a JoinError today; the registry must clean up after it).
        let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.dispatch(
                &PanicHost,
                &Request {
                    v: PROTOCOL_VERSION,
                    id: "r-1".into(),
                    method: "boom.boom".into(),
                    params: json!({ "text": "x" }),
                },
                None,
                None,
            )
        }));
        // The registry converts the panic into a handled error (no unwind).
        let first = first.expect("no unwind escapes dispatch");
        let first_err = first.expect_err("a panicking handler surfaces as an error");
        assert_eq!(first_err.code, "HANDLER_PANIC");

        // After the panic, the same id must NOT be permanently locked as
        // in-flight ("failures are not cached: transient errors may be
        // retried with the same id").
        // The retry RE-RUNS the panicking handler — the panic is caught here
        // too; what must NOT happen is IDEMPOTENT_IN_FLIGHT (permanent lock).
        let second = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.dispatch(
                &PanicHost,
                &Request {
                    v: PROTOCOL_VERSION,
                    id: "r-1".into(),
                    method: "boom.boom".into(),
                    params: json!({ "text": "x" }),
                },
                None,
                None,
            )
        }));
        match second {
            // HANDLER_PANIC (converted): not permanently locked.
            Err(_resumed_panic) => { /* retry re-ran the handler: acceptable */ }
            Ok(Err(err)) => assert_ne!(
                err.code, "IDEMPOTENT_IN_FLIGHT",
                "the id must be retryable after a handler panic (got {err:?})"
            ),
            Ok(Ok(response)) => panic!("panicking handler must not produce Ok: {response:?}"),
        }
    }
}
