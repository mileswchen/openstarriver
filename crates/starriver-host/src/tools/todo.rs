//! `todo_write` tool (`todo` parity): the model maintains the session's
//! todo list; each call replaces the previous list. The rendered list lands
//! in the session log (tool/result), so later turns can read back — and the
//! desktop App parses it into the session's todo card.

use std::sync::Arc;
use std::time::Duration;

use cordis_rslite::Value;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, text_content};

/// Item state: `status` accepts the model-conventional strings
/// (pending/in_progress/completed — the todo vocabulary models are
/// trained on); legacy `checked: bool` still works and a `checked: true` wins
/// over a pending `status`.
fn item_state(item: &serde_json::Value) -> (bool, bool) {
    let checked = item.get("checked").and_then(|c| c.as_bool());
    let status = item
        .get("status")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .map(str::to_ascii_lowercase);
    let completed = checked.unwrap_or(false)
        || matches!(
            status.as_deref(),
            Some("completed") | Some("complete") | Some("done")
        );
    let in_progress = !completed
        && matches!(
            status.as_deref(),
            Some("in_progress") | Some("in-progress") | Some("active") | Some("current")
        );
    (completed, in_progress)
}

impl TodoWrite {
    fn render(items: &[serde_json::Value]) -> Result<String, ToolError> {
        if items.len() > 200 {
            return Err(invalid_input(
                "todo_write",
                "todo list exceeds 200 items".into(),
            ));
        }
        let mut rendered = String::from("## TODO\n");
        for item in items {
            let title = item
                .get("title")
                .and_then(|t| t.as_str())
                .ok_or_else(|| invalid_input("todo_write", "item needs a title".into()))?;
            let description = item.get("description").and_then(|d| d.as_str());
            let (completed, in_progress) = item_state(item);
            let marker = if completed {
                "x"
            } else if in_progress {
                "-"
            } else {
                " "
            };
            let progress_note = if in_progress { " (in progress)" } else { "" };
            rendered.push_str(&format!(
                "- [{marker}] {title}{progress_note}{}\n",
                description
                    .filter(|d| !d.is_empty())
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default(),
            ));
        }
        Ok(rendered)
    }
}

struct TodoWrite;

impl ToolExecutor for TodoWrite {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        Box::pin(async move {
            let json = super::common::json_args(&arguments)?;
            let items = json
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or_else(|| invalid_input("todo_write", "missing items array".into()))?;
            let rendered = Self::render(items)?;
            Ok(Value::new(serde_json::json!({ "output": rendered })))
        })
    }
}

pub fn todo_write_definition() -> ToolDefinition {
    ToolDefinition {
        name: "todo_write".into(),
        description:
            "Replace the session's todo list. Items are checked off as work progresses; the list persists in the session log for later turns and renders on the session's todo card."
                .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string" },
                            "description": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Item state; completed renders checked, in_progress renders an in-progress marker."
                            },
                            "checked": {
                                "type": "boolean",
                                "description": "Legacy boolean spelling of the completed state."
                            }
                        },
                        "required": ["title"]
                    }
                }
            },
            "required": ["items"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(TodoWrite),
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

    fn ctx() -> ToolRunContext {
        let exec = starriver_kernel::tools::ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "todo_write".into(),
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

    fn render_items(items: serde_json::Value) -> String {
        let result =
            block_on(TodoWrite.execute(Value::new(serde_json::json!({ "items": items })), ctx()))
                .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        json["output"].as_str().unwrap().to_string()
    }

    #[test]
    fn renders_checkboxes() {
        let output = render_items(serde_json::json!([
            { "title": "read code", "checked": true },
            { "title": "write tests", "description": "cover edges" }
        ]));
        assert!(output.contains("- [x] read code"), "{output}");
        assert!(
            output.contains("- [ ] write tests — cover edges"),
            "{output}"
        );
    }

    #[test]
    fn status_strings_drive_the_state() {
        let output = render_items(serde_json::json!([
            { "title": "done", "status": "completed" },
            { "title": "working", "status": "in_progress" },
            { "title": "later", "status": "pending" },
            // Legacy checked wins over a conflicting pending status.
            { "title": "legacy", "status": "pending", "checked": true }
        ]));
        assert!(output.contains("- [x] done"), "{output}");
        assert!(output.contains("- [-] working (in progress)"), "{output}");
        assert!(output.contains("- [ ] later"), "{output}");
        assert!(output.contains("- [x] legacy"), "{output}");
    }

    #[test]
    fn rejects_missing_items_or_untitled() {
        let err =
            block_on(TodoWrite.execute(Value::new(serde_json::json!({})), ctx())).unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");
        let err = block_on(TodoWrite.execute(
            Value::new(serde_json::json!({ "items": [{ "description": "no title" }] })),
            ctx(),
        ))
        .unwrap_err();
        assert_eq!(err.code(), "INVALID_INPUT");
    }
}
