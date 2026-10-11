//! `goal_write` tool (`goal` parity): the model declares the session's
//! durable completion objective. A session carries at most one goal; writing
//! one replaces the previous.

use std::sync::Arc;
use std::time::Duration;

use cordis_rslite::Value;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, text_content};

struct GoalWrite;

impl ToolExecutor for GoalWrite {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        Box::pin(async move {
            let json = super::common::json_args(&arguments)?;
            let objective = json
                .get("objective")
                .and_then(|o| o.as_str())
                .filter(|o| !o.is_empty())
                .ok_or_else(|| invalid_input("goal_write", "missing objective".into()))?;
            if objective.chars().count() > 2000 {
                return Err(invalid_input(
                    "goal_write",
                    "objective exceeds 2000 characters".into(),
                ));
            }
            let rendered = format!("## Goal\n{objective}\n");
            Ok(Value::new(serde_json::json!({ "output": rendered })))
        })
    }
}

pub fn goal_write_definition() -> ToolDefinition {
    ToolDefinition {
        name: "goal_write".into(),
        description:
            "Declare the session's single durable completion objective. Writing a goal replaces the previous one; the goal persists in the session log for later turns."
                .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "objective": { "type": "string", "description": "one-sentence completion objective" }
            },
            "required": ["objective"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(GoalWrite),
        is_concurrency_safe: None,
        timeout: Some(Duration::from_secs(10)),
        requires_approval: false,
        deny_in_readonly: false,
        finalize_content: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    fn ctx(name: &str) -> ToolRunContext {
        let exec = starriver_kernel::tools::ToolExecution {
            call_id: starriver_kernel::llm::CallId(name.into()),
            root_call_id: starriver_kernel::llm::CallId(name.into()),
            name: "goal_write".into(),
            arguments: Value::new(()),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        ToolRunContext::new(exec)
    }

    #[test]
    fn renders_goal_and_rejects_empty() {
        let result = block_on(GoalWrite.execute(
            Value::new(serde_json::json!({ "objective": "land the release" })),
            ctx("c1"),
        ))
        .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        assert!(json["output"]
            .as_str()
            .unwrap()
            .contains("land the release"));

        let err =
            block_on(GoalWrite.execute(Value::new(serde_json::json!({})), ctx("c2"))).unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");
    }
}
