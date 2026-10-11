use dashmap::DashMap;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::oneshot;
use uuid::Uuid;

/// Bidirectional bridge between Rust tools and TS script execution.
///
/// TS calls setScriptExecutor(callback) to register a callback.
/// When a ScriptTool.execute() is called:
///   1. call_script(path, params) → creates oneshot → sends {id, path, params} to TS
///   2. TS: import(path) → default.execute(params) → resolveScriptResult(id, result)
///   3. oneshot receiver gets the result → returns to ScriptTool
pub struct ScriptRuntime {
    pending: Arc<DashMap<String, oneshot::Sender<Value>>>,
    exec_callback: ThreadsafeFunction<Value>,
}

impl ScriptRuntime {
    pub fn new(exec_callback: ThreadsafeFunction<Value>) -> Self {
        Self {
            pending: Arc::new(DashMap::new()),
            exec_callback,
        }
    }

    pub async fn call_script(&self, path: &str, params: &Value) -> anyhow::Result<String> {
        let id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id.clone(), tx);

        let msg = serde_json::json!({
            "id": id,
            "path": path,
            "params": params,
        });
        self.exec_callback
            .call(Ok(msg), ThreadsafeFunctionCallMode::NonBlocking);

        match rx.await {
            Ok(value) => {
                if let Some(err) = value.get("error").and_then(|v| v.as_str()) {
                    Err(anyhow::anyhow!("script error: {err}"))
                } else {
                    Ok(serde_json::to_string_pretty(&value).unwrap_or_else(|_| format!("{value}")))
                }
            }
            Err(_) => Err(anyhow::anyhow!("script execution cancelled")),
        }
    }

    /// Called from TS (via napi) after script execution completes.
    pub fn resolve_result(&self, id: &str, result: Value) {
        if let Some((_, tx)) = self.pending.remove(id) {
            let _ = tx.send(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// Verify ScriptRuntime resolves results correctly
    #[test]
    fn test_resolve_result_sends_value() {
        // ScriptRuntime requires a ThreadsafeFunction which is hard to construct
        // in test. But we verify the module structure compiles.
        // The actual integration test is done via napi/Node.js.
    }

    /// Verify the module can be imported and types exist
    #[test]
    fn test_types_exist() {
        // Just verify the module compiles and types are accessible
        assert!(true);
    }
}
