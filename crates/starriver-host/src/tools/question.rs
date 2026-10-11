//! `ask_user` tool (WI-67, interaction · user-questions slice 1): the agent
//! asks the user a question mid-run — optional preset options, bounded wait.
//! The miss (timeout) is reported as tool content, not an error, so the
//! model can adapt its plan instead of failing the turn.

use std::sync::Arc;
use std::time::Duration;

use cordis_rslite::Value;
use futures::future::BoxFuture;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, json_args};

/// Host-provided asker: `(session_id, question, options, timeout)` →
/// `Ok(answer)` or `Err(AskError::as_str())` on timeout/unavailability.
pub type UserAsker = Arc<
    dyn Fn(String, String, Vec<String>, Duration) -> BoxFuture<'static, Result<String, String>>
        + Send
        + Sync,
>;

struct AskUser {
    asker: UserAsker,
}

impl ToolExecutor for AskUser {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let asker = self.asker.clone();
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let question = json
                .get("question")
                .and_then(|q| q.as_str())
                .filter(|q| !q.is_empty())
                .ok_or_else(|| invalid_input("ask_user", "missing question".into()))?
                .to_string();
            let options: Vec<String> = json
                .get("options")
                .and_then(|o| o.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            // timeout_secs: 0 falls back to the default instead of an
            // instant timeout the user can never answer within.
            let timeout = Duration::from_secs(
                json.get("timeout_secs")
                    .and_then(|t| t.as_u64())
                    .filter(|t| *t > 0)
                    .unwrap_or(timeout_secs_default().as_secs()),
            );
            let session_id = ctx
                .execution
                .agent_id
                .clone()
                .ok_or_else(|| invalid_input("ask_user", "no active session".into()))?;
            match asker(session_id, question, options, timeout).await {
                Ok(answer) => Ok(Value::new(serde_json::json!({
                    "status": "answered",
                    "answer": answer,
                }))),
                Err(reason) => {
                    // `unavailable` covers unattended (scheduled) runs, where
                    // no user will ever answer: tell the model to stop asking
                    // instead of retrying and stalling the run.
                    let note = if reason == "unavailable" {
                        "unattended run: no user can answer; do not call ask_user again — decide autonomously and state the assumption in your final answer"
                    } else {
                        "no user answer; proceed with your best judgement and state the assumption"
                    };
                    Ok(Value::new(serde_json::json!({
                        "status": reason,
                        "note": note,
                    })))
                }
            }
        })
    }
}

fn timeout_secs_default() -> Duration {
    crate::question_bridge::DEFAULT_QUESTION_TIMEOUT
}

pub fn ask_user_definition(asker: UserAsker) -> ToolDefinition {
    ToolDefinition {
        name: "ask_user".into(),
        description: "Ask the user a question mid-task when a decision needs their input. \
                      Optionally offer preset options; wait (bounded) for their answer. \
                      On timeout the tool reports it so you can proceed with a stated assumption."
            .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "the question for the user" },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "preset answers rendered as buttons (optional)"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "how long to wait for an answer (default 120)"
                }
            },
            "required": ["question"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "enum": ["answered", "timeout", "unavailable"] },
                "answer": { "type": "string" },
                "note": { "type": "string" }
            }
        })),
        requires_approval: false,
        deny_in_readonly: false,
        render: Arc::new(|_args: &Value, value: &Value| {
            use starriver_kernel::llm::ContentBlock;
            // The executor materialized a serde_json value; read it back
            // through the same type.
            let json: serde_json::Value = value
                .downcast::<serde_json::Value>()
                .map(|arc| (*arc).clone())
                .unwrap_or_else(|_| serde_json::json!({}));
            let status = json
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let text = match status {
                "answered" => {
                    let answer = json
                        .get("answer")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    format!("User answered: {answer}")
                }
                "timeout" => "No user answer (timed out).".to_string(),
                "unavailable" => "No user available (unattended run).".to_string(),
                other => format!("ask_user ended with status `{other}`."),
            };
            vec![ContentBlock::Text(text)]
        }),
        executor: Arc::new(AskUser { asker }),
        is_concurrency_safe: Some(Arc::new(|_args: &Value| false)),
        timeout: None,
        finalize_content: None,
    }
}
