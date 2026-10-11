/// Script tool executor abstraction. Desktop injects a Bun/Node implementation; TUI injects an error stub.
pub trait ScriptExecutor: Send + Sync {
    fn execute(
        &self,
        id: String,
        path: String,
        params: serde_json::Value,
        resolve: Box<dyn Fn(serde_json::Value) + Send + Sync>,
    );
}
