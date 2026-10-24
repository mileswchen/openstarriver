// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! BUG-155: tool-level skill loading.
//!
//! Evidence (harness2 adaptation campaign, ~40 rollouts): prose trigger
//! lines — the only loading channel before this — produced **zero** skill
//! loads across 5+ phrasings, while the same prompts' short unconditional
//! rules were followed 7/7. Loading must be a tool call, not a wish.

use std::sync::Arc;

use cordis_rslite::Value;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};
use starriver_skills::profile::skill_manager::SkillManager;

use super::common::{invalid_input, json_args, text_content, tool_failure};

/// Shared handle to the kernel's skill source — the same cell the host
/// attaches its `SkillManager` to (`PluginKernel::skills_cell`). `None`
/// until the host attaches; executors surface that as `SKILLS_UNAVAILABLE`.
pub type SkillsCell = Arc<std::sync::Mutex<Option<Arc<SkillManager>>>>;

fn attached_manager(cell: &SkillsCell) -> Result<Arc<SkillManager>, ToolError> {
    cell.lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| {
            tool_failure(
                "skill_list",
                "SKILLS_UNAVAILABLE",
                "no skill manager is attached to this kernel".into(),
            )
        })
}

pub fn guide() -> (&'static str, i32, &'static str) {
    (
        "skill_list",
        118,
        "Before re-deriving project conventions or verification contracts by \
         hand, check skill_list — a distilled skill may already exist; load it \
         with skill_load and follow it.",
    )
}

struct SkillList {
    cell: SkillsCell,
}

impl ToolExecutor for SkillList {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let cell = self.cell.clone();
        Box::pin(async move {
            let _ = json_args(&arguments)?;
            let manager = attached_manager(&cell)?;
            let mut skills = manager.list_all();
            skills.sort_by(|a, b| a.name.cmp(&b.name));
            let lines: Vec<String> = skills
                .iter()
                .map(|s| {
                    let description = s.description.as_deref().unwrap_or("");
                    if s.always_on {
                        format!("- {} — {description} (always-on)", s.name)
                    } else {
                        format!("- {} — {description}", s.name)
                    }
                })
                .collect();
            let output = if lines.is_empty() {
                "No skills are installed.".to_string()
            } else {
                lines.join("\n")
            };
            Ok(Value::new(serde_json::json!({ "output": output })))
        })
    }
}

struct SkillLoad {
    cell: SkillsCell,
}

impl ToolExecutor for SkillLoad {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        let cell = self.cell.clone();
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let name = json
                .get("name")
                .and_then(|n| n.as_str())
                .ok_or_else(|| invalid_input("skill_load", "missing name".into()))?
                .to_string();
            let manager = attached_manager(&cell)?;
            let Some(skill) = manager.get(&name) else {
                let known = manager.list_all();
                let mut names: Vec<&str> = known.iter().map(|s| s.name.as_str()).collect();
                names.sort_unstable();
                return Err(tool_failure(
                    "skill_load",
                    "SKILL_NOT_FOUND",
                    format!("no skill named `{name}` — skill_list shows: {names:?}"),
                ));
            };
            // Observable so adoption can be measured against the 0-load
            // baseline (BUG-155 acceptance).
            tracing::info!(skill = %skill.name, "skill loaded via skill_load tool");
            let header = match &skill.description {
                Some(description) => format!("# {} — {description}\n\n", skill.name),
                None => format!("# {}\n\n", skill.name),
            };
            let mut output = format!("{header}{}", skill.body);
            if skill.always_on {
                output.push_str("\n\n(This skill is always-on; you already had its body.)");
            }
            Ok(Value::new(serde_json::json!({ "output": output })))
        })
    }
}

pub fn skill_list_definition(cell: SkillsCell) -> ToolDefinition {
    ToolDefinition {
        name: "skill_list".into(),
        description: "List installed skills (name + one-line description). Check this before \
                      hand-deriving conventions a skill may already encode."
            .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {},
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(SkillList { cell }),
        is_concurrency_safe: None,
        timeout: None,
        finalize_content: None,
        requires_approval: false,
        deny_in_readonly: false,
    }
}

pub fn skill_load_definition(cell: SkillsCell) -> ToolDefinition {
    ToolDefinition {
        name: "skill_load".into(),
        description: "Load one skill's full instructions into the conversation. One at a time; \
                      read it and apply it before continuing."
            .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "skill name from skill_list" }
            },
            "required": ["name"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(SkillLoad { cell }),
        is_concurrency_safe: None,
        timeout: None,
        finalize_content: None,
        requires_approval: false,
        deny_in_readonly: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::tools::ToolExecution;

    /// Fixture: a temp skills dir with one skill, wrapped in the same
    /// `SkillsCell` shape the host hands the kernel.
    fn fixture_cell() -> (SkillsCell, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("restore-stubbed-function");
        std::fs::create_dir_all(&sub).expect("mkdir");
        std::fs::write(
            sub.join("SKILL.md"),
            "---\nname: restore-stubbed-function\ndescription: Contract for restoring a stubbed pure function\n---\n\n## Steps\n1. Read the tests\n2. Restore the body\n",
        )
        .expect("write SKILL.md");
        let manager = Arc::new(SkillManager::new());
        manager.load_dir(dir.path()).expect("load skills");
        let cell: SkillsCell = Arc::new(std::sync::Mutex::new(Some(manager)));
        (cell, dir)
    }

    fn execution(command_json: serde_json::Value) -> (Value, ToolRunContext) {
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "skill_list".into(),
            arguments: Value::new(serde_json::json!({})),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        (
            Value::new(command_json),
            ToolRunContext::new(execution),
        )
    }

    async fn run_list(cell: SkillsCell) -> String {
        let (args, ctx) = execution(serde_json::json!({}));
        let result = skill_list_definition(cell)
            .executor
            .execute(args, ctx)
            .await
            .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();
        json["output"].as_str().unwrap().to_string()
    }

    async fn run_load(cell: SkillsCell, name: &str) -> Result<String, ToolError> {
        let (args, ctx) = execution(serde_json::json!({ "name": name }));
        let result = skill_load_definition(cell)
            .executor
            .execute(args, ctx)
            .await?;
        let json = result.downcast::<serde_json::Value>().unwrap();
        Ok(json["output"].as_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn skill_list_names_installed_skills() {
        let (cell, _dir) = fixture_cell();
        let out = run_list(cell).await;
        assert!(out.contains("restore-stubbed-function"), "got: {out}");
        assert!(out.contains("Contract for restoring"), "got: {out}");
    }

    #[tokio::test]
    async fn skill_load_returns_the_body_and_not_found_carries_suggestions() {
        let (cell, _dir) = fixture_cell();
        let out = run_load(cell.clone(), "restore-stubbed-function")
            .await
            .expect("load succeeds");
        assert!(out.contains("Read the tests"), "body missing: {out}");
        assert!(out.contains("restore-stubbed-function"), "header missing");

        let err = run_load(cell, "no-such-skill").await.expect_err("must fail");
        assert_eq!(err.code(), "SKILL_NOT_FOUND");
        assert!(
            err.to_string().contains("restore-stubbed-function"),
            "error should list known skills: {err}"
        );
    }

    /// Without an attached manager (kernel not yet host-attached) both tools
    /// fail loudly instead of pretending success.
    #[tokio::test]
    async fn empty_cell_reports_skills_unavailable() {
        let cell: SkillsCell = Arc::new(std::sync::Mutex::new(None));
        let (args, ctx) = execution(serde_json::json!({}));
        let err = skill_list_definition(cell.clone())
            .executor
            .execute(args, ctx)
            .await
            .expect_err("must fail");
        assert_eq!(err.code(), "SKILLS_UNAVAILABLE");
        let (args, ctx) = execution(serde_json::json!({ "name": "x" }));
        let err = skill_load_definition(cell)
            .executor
            .execute(args, ctx)
            .await
            .expect_err("must fail");
        assert_eq!(err.code(), "SKILLS_UNAVAILABLE");
    }

    #[test]
    fn guide_stays_inside_the_reserved_band() {
        let (tool, order, text) = guide();
        assert_eq!(tool, "skill_list");
        assert!((100..=199).contains(&order), "order {order} outside band");
        assert!(!text.trim().is_empty());
    }
}
