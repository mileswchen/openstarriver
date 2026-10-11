//! `plan_write` tool (`plan` parity): in plan mode the model produces a
//! design before executing; the plan is persisted in the session log and the
//! UI surfaces it for confirmation before execution.

use std::sync::Arc;
use std::time::Duration;

use cordis_rslite::Value;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, text_content};

struct PlanWrite;

impl ToolExecutor for PlanWrite {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        Box::pin(async move {
            let json = super::common::json_args(&arguments)?;
            let plan = json
                .get("plan")
                .and_then(|p| p.as_str())
                .filter(|p| !p.is_empty())
                .ok_or_else(|| invalid_input("plan_write", "missing plan".into()))?;
            if plan.chars().count() > 20_000 {
                return Err(invalid_input(
                    "plan_write",
                    "plan exceeds 20000 characters".into(),
                ));
            }
            let rendered = format!("## Plan\n{plan}\n");
            Ok(Value::new(serde_json::json!({ "output": rendered })))
        })
    }
}

pub fn plan_write_definition() -> ToolDefinition {
    ToolDefinition {
        name: "plan_write".into(),
        description:
            "Write the design for the current task before executing. The plan is persisted in the session log for the user to confirm; call it once in plan mode, then wait."
                .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "plan": { "type": "string", "description": "markdown design / step list" }
            },
            "required": ["plan"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(PlanWrite),
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

    #[test]
    fn renders_plan_and_rejects_empty() {
        let exec = starriver_kernel::tools::ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "plan_write".into(),
            arguments: Value::new(()),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(exec);
        let result = block_on(PlanWrite.execute(
            Value::new(serde_json::json!({ "plan": "1. explore\n2. build" })),
            ctx,
        ))
        .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        assert!(json["output"].as_str().unwrap().contains("1. explore"));

        let exec = starriver_kernel::tools::ToolExecution {
            call_id: starriver_kernel::llm::CallId("c2".into()),
            root_call_id: starriver_kernel::llm::CallId("c2".into()),
            name: "plan_write".into(),
            arguments: Value::new(()),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        let ctx = ToolRunContext::new(exec);
        let err = block_on(PlanWrite.execute(Value::new(serde_json::json!({})), ctx)).unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");
    }
}
