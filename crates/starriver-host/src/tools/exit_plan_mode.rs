//! `exit_plan_mode` tool (plan fold, plan-mode parity): the model
//! submits its complete plan for review. Approval records a silent pending
//! exit (landed at the next accepted in-turn pre-step by the plan
//! controller) and returns `{ approved: true }`; keep-planning is a failed
//! call carrying the user's feedback, so the model revises and presents
//! again. The tool stays registered while plan mode is inactive — entering
//! or leaving plan mode changes only the prompt section, never the tool
//! catalog — and since WI-178 the review bridge answers in standard mode
//! too: outside plan mode an approval simply resumes the model (the
//! controller's inactive write is idempotent, no plan-mode state flips).

use std::sync::Arc;

use cordis_rslite::Value;
use futures::future::BoxFuture;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, json_args, tool_failure};

/// The reviewer's verdict on a presented plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanReviewVerdict {
    Approved,
    Rejected { feedback: Option<String> },
}

/// Host-provided reviewer: `(session_id, call_id, plan)` → verdict, or
/// `Err(tag)` with `timeout` / `unavailable` / `not-in-plan-mode`.
pub type PlanReviewer = Arc<
    dyn Fn(String, Option<String>, String) -> BoxFuture<'static, Result<PlanReviewVerdict, String>>
        + Send
        + Sync,
>;

struct ExitPlanMode {
    reviewer: PlanReviewer,
}

impl ToolExecutor for ExitPlanMode {
    fn execute(
        &self,
        arguments: Value,
        ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let reviewer = Arc::clone(&self.reviewer);
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let plan = json
                .get("plan")
                .and_then(|p| p.as_str())
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| invalid_input("exit_plan_mode", "missing plan".into()))?
                .to_string();
 //: a complete markdown plan starts with a `#` heading.
            if !plan.starts_with('#') {
                return Err(invalid_input(
                    "exit_plan_mode",
                    "plan must be complete markdown starting with a `#` heading".into(),
                ));
            }
            let session_id = ctx
                .execution
                .agent_id
                .clone()
                .ok_or_else(|| invalid_input("exit_plan_mode", "no active session".into()))?;
            let call_id = Some(ctx.execution.call_id.0.clone());
            match reviewer(session_id, call_id, plan).await {
                Ok(PlanReviewVerdict::Approved) => Ok(Value::new(serde_json::json!({
                    "approved": true,
                }))),
                Ok(PlanReviewVerdict::Rejected { feedback }) => {
                    Err(rejection_error(feedback.as_deref()))
                }
                Err(tag) => Err(match tag.as_str() {
                    // Defensive: the bridge answers in every mode now (WI-178);
                    // this arm only fires if a host reintroduces the gate.
                    "not-in-plan-mode" => tool_failure(
                        "exit_plan_mode",
                        "NOT_IN_PLAN_MODE",
                        "plan mode is not active; exit_plan_mode only runs while plan mode is active"
                            .into(),
                    ),
                    "timeout" => tool_failure(
                        "exit_plan_mode",
                        "PLAN_REVIEW_TIMEOUT",
                        "plan review timed out without a decision; you may present the plan again"
                            .into(),
                    ),
                    other => tool_failure(
                        "exit_plan_mode",
                        "REVIEW_UNAVAILABLE",
                        format!("plan review unavailable ({other}); no decision was recorded"),
                    ),
                }),
            }
        })
    }
}

/// Keep-planning is a failed call carrying the user's feedback:
/// the model revises and presents again instead of leaving plan mode.
fn rejection_error(feedback: Option<&str>) -> ToolError {
    let message = match feedback.map(str::trim).filter(|f| !f.is_empty()) {
        Some(feedback) => format!(
            "The user reviewed the plan and chose to keep planning. Feedback: {feedback}\n\
             Revise the plan accordingly and present it again with exit_plan_mode."
        ),
        None => String::from(
            "The user reviewed the plan and chose to keep planning (no feedback given).\n\
             Revise the plan and present it again with exit_plan_mode.",
        ),
    };
    tool_failure("exit_plan_mode", "PLAN_REJECTED", message)
}

pub fn exit_plan_mode_definition(reviewer: PlanReviewer) -> ToolDefinition {
    ToolDefinition {
        name: "exit_plan_mode".into(),
        description: "Present your complete implementation plan for the user's review and \
                      request permission to proceed with execution. Parameters: plan \
                      (complete markdown, starting with a `#` heading). Approval exits plan \
                      mode; rejection fails the call and carries the user's feedback so you \
                      can revise and present again."
            .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "plan": {
                    "type": "string",
                    "description": "the complete plan as markdown, starting with a `#` heading"
                }
            },
            "required": ["plan"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "approved": { "type": "boolean" } }
        })),
        render: Arc::new(|_canonical: &Value, value: &Value| {
            use starriver_kernel::llm::ContentBlock;
            let approved = value
                .downcast::<serde_json::Value>()
                .ok()
                .and_then(|v| v.get("approved").and_then(serde_json::Value::as_bool))
                .unwrap_or(false);
            let text = if approved {
                "Plan approved. Plan mode is being exited — proceed with implementation."
                    .to_string()
            } else {
                "Plan review ended without approval.".to_string()
            };
            vec![ContentBlock::Text(text)]
        }),
        executor: Arc::new(ExitPlanMode { reviewer }),
        // The review IS the interaction; the approval waterfall must not
        // double-prompt, and the read-only refusal must never block it.
        requires_approval: false,
        deny_in_readonly: false,
        // Exclusive: a plan review never runs alongside other calls.
        is_concurrency_safe: Some(Arc::new(|_args: &Value| false)),
        timeout: None,
        finalize_content: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::llm::CallId;
    use starriver_kernel::tools::{mint_tool_execution_token, ToolExecution};

    fn run_with(
        reviewer: PlanReviewer,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let definition = exit_plan_mode_definition(reviewer);
        let ctx = ToolRunContext::new(ToolExecution {
            call_id: CallId("c1".into()),
            root_call_id: CallId("c1".into()),
            name: "exit_plan_mode".into(),
            arguments: Value::new(args.clone()),
            agent_id: Some("s1".into()),
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: mint_tool_execution_token(),
            cancellation: None,
        });
        let fut = definition.executor.execute(Value::new(args), ctx);
        futures::executor::block_on(fut).map(|value| {
            value
                .downcast::<serde_json::Value>()
                .map(|arc| (*arc).clone())
                .expect("executor returns serde_json payload")
        })
    }

    #[test]
    fn approval_returns_approved_true() {
        let reviewer: PlanReviewer = Arc::new(|_s, _c, _p| {
            Box::pin(async { Ok(PlanReviewVerdict::Approved) }) as BoxFuture<_>
        });
        let out = run_with(reviewer, serde_json::json!({ "plan": "# Plan\nsteps" })).unwrap();
        assert_eq!(out["approved"], true);
    }

    #[test]
    fn rejection_fails_with_the_feedback() {
        let reviewer: PlanReviewer = Arc::new(|_s, _c, _p| {
            Box::pin(async {
                Ok(PlanReviewVerdict::Rejected {
                    feedback: Some("skip the migration".into()),
                })
            }) as BoxFuture<_>
        });
        let err = run_with(reviewer, serde_json::json!({ "plan": "# Plan" })).unwrap_err();
        match err {
            ToolError::Execution { code, message, .. } => {
                assert_eq!(code, "PLAN_REJECTED");
                assert!(message.contains("skip the migration"), "{message}");
            }
            other => panic!("expected execution error, got {other:?}"),
        }
    }

    #[test]
    fn missing_heading_and_missing_plan_are_invalid_input() {
        let reviewer: PlanReviewer = Arc::new(|_s, _c, _p| {
            Box::pin(async { Ok(PlanReviewVerdict::Approved) }) as BoxFuture<_>
        });
        for args in [
            serde_json::json!({ "plan": "just prose" }),
            serde_json::json!({ "plan": "" }),
            serde_json::json!({}),
        ] {
            let err = run_with(reviewer.clone(), args).unwrap_err();
            assert!(
                matches!(err, ToolError::Execution { ref code, .. } if code == "INVALID_INPUT"),
                "{err:?}"
            );
        }
    }

    #[test]
    fn reviewer_tags_map_to_stable_codes() {
        for (tag, expected) in [
            ("not-in-plan-mode", "NOT_IN_PLAN_MODE"),
            ("timeout", "PLAN_REVIEW_TIMEOUT"),
            ("unavailable", "REVIEW_UNAVAILABLE"),
        ] {
            let reviewer: PlanReviewer = Arc::new(move |_s, _c, _p| {
                let tag = tag.to_string();
                Box::pin(async move { Err(tag) }) as BoxFuture<_>
            });
            let err = run_with(reviewer, serde_json::json!({ "plan": "# P" })).unwrap_err();
            assert!(
                matches!(err, ToolError::Execution { ref code, .. } if code == expected),
                "tag `{tag}` → {err:?}"
            );
        }
    }

    #[test]
    fn definition_shape_is_exclusive_and_readonly_safe() {
        let reviewer: PlanReviewer = Arc::new(|_s, _c, _p| {
            Box::pin(async { Ok(PlanReviewVerdict::Approved) }) as BoxFuture<_>
        });
        let definition = exit_plan_mode_definition(reviewer);
        assert_eq!(definition.name, "exit_plan_mode");
        assert!(
            !definition.requires_approval,
            "the review is the interaction"
        );
        assert!(
            !definition.deny_in_readonly,
            "must run under the read-only pairing"
        );
        let args = Value::new(serde_json::json!({ "plan": "# x" }));
        assert_eq!(
            (definition.is_concurrency_safe.as_ref().unwrap())(&args),
            false,
            "plan reviews are exclusive"
        );
    }
}
