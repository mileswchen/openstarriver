//! `subagent_spawn` tool (subagent in-process driver parity): delegates a
//! bounded task to a configured subagent template (explorer / tester /
//! reviewer) running under its bound child model.

use std::sync::Arc;
use std::time::Duration;

use cordis_rslite::Value;
use futures::future::BoxFuture;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, json_args, text_content, tool_failure};

/// Host-provided subagent runner: `(template, prompt, caller_session_id)` →
/// final text. The caller id lets the host inherit the calling agent's route
/// when the template binds none (parent-inheritance parity).
pub type SubagentSpawner = Arc<
    dyn Fn(String, String, Option<String>) -> BoxFuture<'static, Result<String, String>>
        + Send
        + Sync,
>;

/// Hot read of the currently enabled template names (settings-backed). Sampled
/// for the model-facing description at registration and re-checked at every
/// call, mirroring two-layer enabled gate.
pub type EnabledTemplates = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

struct SubagentSpawn {
    spawner: SubagentSpawner,
    enabled: EnabledTemplates,
}

impl ToolExecutor for SubagentSpawn {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let spawner = self.spawner.clone();
        let enabled = self.enabled.clone();
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let template = json
                .get("template")
                .and_then(|t| t.as_str())
                .ok_or_else(|| invalid_input("subagent_spawn", "missing template".into()))?
                .to_string();
            let prompt = json
                .get("prompt")
                .and_then(|p| p.as_str())
                .filter(|p| !p.is_empty())
                .ok_or_else(|| invalid_input("subagent_spawn", "missing prompt".into()))?
                .to_string();
            if !enabled().iter().any(|name| name == &template) {
                return Err(invalid_input(
                    "subagent_spawn",
                    format!("unknown or disabled subagent template `{template}`"),
                ));
            }
            let output = spawner(template.clone(), prompt, ctx.execution.agent_id.clone())
                .await
                .map_err(|e| tool_failure("subagent_spawn", "SUBAGENT_FAILED", e))?;
            let rendered = format!("## Subagent ({template})\n{output}\n");
            Ok(Value::new(serde_json::json!({ "output": rendered })))
        })
    }
}

pub fn subagent_spawn_definition(
    spawner: SubagentSpawner,
    enabled: EnabledTemplates,
) -> ToolDefinition {
    let enabled_names = enabled().join(", ");
    ToolDefinition {
        name: "subagent_spawn".into(),
        description: format!(
            "Delegate a bounded task to an enabled subagent template ({}). The subagent runs in a fresh session under its bound model, or inherits the calling agent's route when none is bound, and returns its final text.",
            if enabled_names.is_empty() { "none enabled — enable one in settings first".to_string() } else { enabled_names }
        ),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "template": { "type": "string", "description": "subagent template name" },
                "prompt": { "type": "string", "description": "bounded task for the subagent" }
            },
            "required": ["template", "prompt"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(SubagentSpawn { spawner, enabled }),
        is_concurrency_safe: None,
        timeout: Some(Duration::from_secs(600)),
        requires_approval: true,
        deny_in_readonly: false,
        finalize_content: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use std::sync::Mutex;

    fn enabled_of(names: &[&str]) -> EnabledTemplates {
        let owned: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        Arc::new(move || owned.clone())
    }

    fn ctx() -> ToolRunContext {
        let exec = starriver_kernel::tools::ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "subagent_spawn".into(),
            arguments: Value::new(()),
            agent_id: Some("parent-session".into()),
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        ToolRunContext::new(exec)
    }

    #[test]
    fn delegates_to_the_spawner_and_renders_result() {
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        let spawner: SubagentSpawner = Arc::new(move |template, prompt, caller| {
            seen2.lock().unwrap().push(caller);
            Box::pin(async move { Ok(format!("{template} handled: {prompt}")) })
        });
        let def = subagent_spawn_definition(spawner, enabled_of(&["explorer"]));
        let result = block_on(def.executor.execute(
            Value::new(serde_json::json!({ "template": "explorer", "prompt": "map the crate" })),
            ctx(),
        ))
        .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        assert!(json["output"]
            .as_str()
            .unwrap()
            .contains("explorer handled: map the crate"));
        // The calling session id reaches the host spawner for route inheritance.
        assert_eq!(
            seen.lock().unwrap().last().unwrap(),
            &Some("parent-session".into())
        );
    }

    #[test]
    fn rejects_unknown_templates_and_missing_prompt() {
        let spawner: SubagentSpawner = Arc::new(|_, _, _| Box::pin(async { Ok(String::new()) }));
        let def = subagent_spawn_definition(spawner, enabled_of(&["explorer"]));
        let run = |args: serde_json::Value| block_on(def.executor.execute(Value::new(args), ctx()));
        let err = run(serde_json::json!({ "template": "ghost", "prompt": "x" })).unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");
        let err = run(serde_json::json!({ "template": "explorer" })).unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");
    }

    #[test]
    fn rejects_disabled_templates_even_though_builtin() {
        let spawner: SubagentSpawner = Arc::new(|_, _, _| Box::pin(async { Ok(String::new()) }));
        // reviewer is a builtin template but not in the enabled set.
        let def = subagent_spawn_definition(spawner, enabled_of(&["explorer", "tester"]));
        let err = block_on(def.executor.execute(
            Value::new(serde_json::json!({ "template": "reviewer", "prompt": "review" })),
            ctx(),
        ))
        .unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");
        assert!(err.to_string().contains("disabled"));
    }

    #[test]
    fn description_lists_only_enabled_templates() {
        let spawner: SubagentSpawner = Arc::new(|_, _, _| Box::pin(async { Ok(String::new()) }));
        let def = subagent_spawn_definition(spawner.clone(), enabled_of(&["explorer"]));
        assert!(def.description.contains("explorer"));
        assert!(!def.description.contains("reviewer"));
        let def = subagent_spawn_definition(spawner, enabled_of(&[]));
        assert!(def.description.contains("none enabled"));
    }
}
