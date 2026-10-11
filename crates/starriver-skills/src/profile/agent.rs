use crate::profile::parser::RawDocument;
use crate::profile::types::*;
use serde::Deserialize;
use starriver_memory::MemoryConfig;

/// The session-composition surface an AGENT.md may pin for its preset
/// (agent_preset alignment: the preset owns the model-visible face —
/// sampling, tool whitelist, prompt assembly — never the model route or the
/// permission planes; `deny_unknown_fields` turns a `model:`/`sandbox:`-style
/// key inside this block into a parse error, which the host surfaces as a
/// `broken` roster row rather than a silent ignore).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPresetSpec {
    pub max_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub time_refresh_interval_ms: Option<u64>,
    pub visible_tools: Option<Vec<String>>,
    pub prompt: Option<AgentPromptPreset>,
}

/// Prompt-assembly overrides folded into the kernel `PromptSettings`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPromptPreset {
    pub include_identity: Option<bool>,
    pub include_runtime_context: Option<bool>,
    pub instructions_max_bytes: Option<u64>,
}

/// Agent configuration profile (AGENT.md)
#[derive(Debug, Clone)]
pub struct AgentProfile {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub author: Option<String>,
    pub icon: Option<String>,
    pub skills: Vec<String>,
    pub tools: Vec<ToolRef>,
    /// Default execution mode applied on switch_agent: "plan" | "build" | null (AUTO).
    pub mode: Option<String>,
    pub model: Option<ModelConfig>,
    pub permissions: Option<PermissionConfig>,
    pub hooks: Option<HookConfig>,
    pub commands: Vec<SlashCommand>,
    pub ui: Option<UiConfig>,
    /// Memory knobs for the cross-session memory layer (Coding Agent). Absent =
    /// `MemoryConfig::default()`. See docs/36-memory-system-design.md §9.
    pub memory: Option<MemoryConfig>,
    /// Per-agent default for new-session worktree isolation: `isolate: true`
    /// runs sessions in a per-session git worktree unless the UI toggle
    /// overrides it. Absent = off.
    pub isolate: Option<bool>,
    /// The `preset:` block: `None` when absent, `Ok(spec)` when valid,
    /// `Err(reason)` when the block is malformed or carries a forbidden key —
    /// the broken reason surfaces on the agent-preset roster (visible, not
    /// selectable) instead of silently degrading.
    pub preset: Option<Result<AgentPresetSpec, String>>,
    pub body: String,
}

impl TryFrom<RawDocument> for AgentProfile {
    type Error = ProfileError;

    fn try_from(doc: RawDocument) -> Result<Self, Self::Error> {
        let fm = &doc.frontmatter;

        let name = fm
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or(ProfileError::MissingField("name"))?;

        if name.is_empty() {
            return Err(ProfileError::InvalidField(
                "name",
                "cannot be empty".to_string(),
            ));
        }

        let version = fm.get("version").and_then(scalar_str);
        let description = fm.get("description").and_then(scalar_str);
        let author = fm.get("author").and_then(scalar_str);
        let icon = fm.get("icon").and_then(scalar_str);

        let skills: Vec<String> = fm
            .get("skills")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let tools: Vec<ToolRef> = fm
            .get("tools")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().and_then(|s| s.parse::<ToolRef>().ok()))
                    .collect()
            })
            .unwrap_or_default();

        let mode = fm.get("mode").and_then(scalar_str);

        let model = fm
            .get("model")
            .and_then(|v| serde_yaml::from_value(v.clone()).ok());

        let permissions = fm
            .get("permissions")
            .and_then(|v| serde_yaml::from_value(v.clone()).ok());

        let hooks = fm
            .get("hooks")
            .and_then(|v| serde_yaml::from_value(v.clone()).ok());

        let commands: Vec<SlashCommand> = fm
            .get("commands")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| serde_yaml::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();

        let ui = fm
            .get("ui")
            .and_then(|v| serde_yaml::from_value(v.clone()).ok());

        let memory = fm
            .get("memory")
            .and_then(|v| serde_yaml::from_value(v.clone()).ok());

        let isolate = fm.get("isolate").and_then(|v| v.as_bool());

        // The `preset:` block hard-rejects: unlike the other optional
        // sections (invalid YAML → silent fallback), a malformed preset is a
        // user-visible broken row — an invalid composition must never
        // quietly start an agent.
        let preset = match fm.get("preset") {
            None => None,
            Some(value) => match serde_yaml::from_value::<AgentPresetSpec>(value.clone()) {
                Ok(spec) => Some(Ok(spec)),
                Err(error) => Some(Err(format!("invalid preset block: {error}"))),
            },
        };

        Ok(AgentProfile {
            name,
            version,
            description,
            author,
            icon,
            skills,
            tools,
            mode,
            model,
            permissions,
            hooks,
            commands,
            ui,
            memory,
            isolate,
            preset,
            body: doc.body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::parser::parse_document;

    fn agent_doc(yaml: &str, body: &str) -> RawDocument {
        parse_document(&format!("---\n{yaml}\n---\n{body}")).unwrap()
    }

    #[test]
    fn parses_full_agent_profile() {
        let doc = agent_doc(
            r#"name: coder
version: "2.0"
description: Coding agent
author: chen
icon: rocket
skills:
  - rust
  - python
tools:
  - builtin:read
  - script:foo
mode: build
model:
  default: gpt4
  models: [m1]
permissions:
  default: deny
  rules:
    - tool: [bash]
      action: ask
hooks:
  postToolResult:
    - matcher: [bash]
commands:
  - name: hello
    description: Say hi
ui:
  color: blue
  greeting: hi
"#,
            "body\n",
        );
        let a = AgentProfile::try_from(doc).unwrap();
        assert_eq!(a.name, "coder");
        assert_eq!(a.version.as_deref(), Some("2.0"));
        assert_eq!(a.description.as_deref(), Some("Coding agent"));
        assert_eq!(a.author.as_deref(), Some("chen"));
        assert_eq!(a.icon.as_deref(), Some("rocket"));
        assert_eq!(a.skills, vec!["rust", "python"]);
        assert_eq!(
            a.tools,
            vec![
                ToolRef::Builtin("read".into()),
                ToolRef::Script("foo".into())
            ]
        );
        assert_eq!(a.mode.as_deref(), Some("build"));
        assert_eq!(a.model.unwrap().default, "gpt4");
        assert_eq!(a.permissions.unwrap().default, PermissionAction::Deny);
        assert_eq!(a.hooks.unwrap().post_tool_result.len(), 1);
        assert_eq!(a.commands.len(), 1);
        assert_eq!(a.commands[0].name, "hello");
        let ui = a.ui.unwrap();
        assert_eq!(ui.color.as_deref(), Some("blue"));
        assert_eq!(ui.greeting.as_deref(), Some("hi"));
        // parse_document trims body whitespace
        assert_eq!(a.body, "body");
    }

    #[test]
    fn missing_name_is_error() {
        let doc = agent_doc("version: 1.0", "b");
        let err = AgentProfile::try_from(doc).unwrap_err();
        assert!(
            matches!(err, ProfileError::MissingField("name")),
            "got: {err}"
        );
    }

    #[test]
    fn unquoted_numeric_version_is_preserved() {
        // YAML parses `version: 2.0` as a float; scalar_str converts it back.
        let doc = agent_doc("name: x\nversion: 2.0", "b");
        let a = AgentProfile::try_from(doc).unwrap();
        assert_eq!(a.version.as_deref(), Some("2.0"));
    }

    #[test]
    fn unquoted_numeric_description_and_author_are_preserved() {
        let doc = agent_doc("name: x\ndescription: 123\nauthor: true", "b");
        let a = AgentProfile::try_from(doc).unwrap();
        assert_eq!(a.description.as_deref(), Some("123"));
        assert_eq!(a.author.as_deref(), Some("true"));
    }

    #[test]
    fn name_as_non_string_is_missing() {
        let doc = agent_doc("name: 42", "b");
        let err = AgentProfile::try_from(doc).unwrap_err();
        assert!(matches!(err, ProfileError::MissingField("name")));
    }

    #[test]
    fn empty_name_is_invalid_field() {
        let doc = agent_doc("name: \"\"", "b");
        let err = AgentProfile::try_from(doc).unwrap_err();
        assert!(
            matches!(err, ProfileError::InvalidField("name", _)),
            "got: {err}"
        );
    }

    #[test]
    fn all_optional_fields_default() {
        let doc = agent_doc("name: bare", "b");
        let a = AgentProfile::try_from(doc).unwrap();
        assert_eq!(a.name, "bare");
        assert!(a.version.is_none());
        assert!(a.description.is_none());
        assert!(a.author.is_none());
        assert!(a.icon.is_none());
        assert!(a.skills.is_empty());
        assert!(a.tools.is_empty());
        assert!(a.mode.is_none());
        assert!(a.model.is_none());
        assert!(a.permissions.is_none());
        assert!(a.hooks.is_none());
        assert!(a.commands.is_empty());
        assert!(a.ui.is_none());
        assert!(a.memory.is_none());
        assert!(a.isolate.is_none());
        assert_eq!(a.body, "b");
    }

    #[test]
    fn parses_isolate_flag() {
        let on = AgentProfile::try_from(agent_doc("name: x\nisolate: true", "b")).unwrap();
        assert_eq!(on.isolate, Some(true));
        let off = AgentProfile::try_from(agent_doc("name: x\nisolate: false", "b")).unwrap();
        assert_eq!(off.isolate, Some(false));
    }

    #[test]
    fn non_bool_isolate_falls_back_to_none() {
        let a = AgentProfile::try_from(agent_doc("name: x\nisolate: \"yes\"", "b")).unwrap();
        assert!(a.isolate.is_none(), "only YAML booleans are honoured");
    }

    #[test]
    fn preset_block_parses_declared_fields() {
        let doc = agent_doc(
            r#"name: coder
preset:
  max_tokens: 32768
  context_window: 131072
  time_refresh_interval_ms: 60000
  visible_tools: [read, grep]
  prompt:
    include_identity: true
    instructions_max_bytes: 65536
"#,
            "body\n",
        );
        let a = AgentProfile::try_from(doc).unwrap();
        let spec = a.preset.expect("present").expect("valid");
        assert_eq!(spec.max_tokens, Some(32768));
        assert_eq!(spec.context_window, Some(131072));
        assert_eq!(spec.time_refresh_interval_ms, Some(60000));
        assert_eq!(
            spec.visible_tools,
            Some(vec!["read".to_string(), "grep".to_string()])
        );
        let prompt = spec.prompt.unwrap();
        assert_eq!(prompt.include_identity, Some(true));
        assert_eq!(prompt.instructions_max_bytes, Some(65536));
        assert_eq!(prompt.include_runtime_context, None);
    }

    #[test]
    fn absent_preset_block_is_none() {
        let a = AgentProfile::try_from(agent_doc("name: bare", "b")).unwrap();
        assert!(a.preset.is_none());
    }

    #[test]
    fn forbidden_preset_key_is_broken_not_ignored() {
 // The route plane must never ride the preset block (excludes the
        // model route from presets) — deny_unknown_fields turns it into a
        // broken reason the roster shows.
        for yaml in [
            "name: x\npreset:\n  model: gpt4\n",
            "name: x\npreset:\n  provider: openai\n",
            "name: x\npreset:\n  sandbox: read-only\n",
            "name: x\npreset:\n  max_tokens: not-a-number\n",
        ] {
            let a = AgentProfile::try_from(agent_doc(yaml, "b")).unwrap();
            match a.preset {
                Some(Err(reason)) => assert!(reason.contains("invalid preset block"), "{reason}"),
                other => panic!("expected broken preset for {yaml}, got {other:?}"),
            }
        }
    }

    #[test]
    fn parses_memory_section_overrides() {
        let doc = agent_doc(
            r#"name: coder
memory:
  llm_select: true
  top_k: 20
  per_project_cap: 50
"#,
            "body\n",
        );
        let a = AgentProfile::try_from(doc).unwrap();
        let m = a.memory.unwrap();
        assert!(m.llm_select, "present key must override");
        assert_eq!(m.top_k, 20);
        assert_eq!(m.per_project_cap, 50);
        // absent keys fall back to MemoryConfig::default()
        assert!(m.enabled);
        assert_eq!(m.inject_budget_tokens, 4096);
        assert_eq!(m.extract_model, None);
    }

    #[test]
    fn invalid_memory_yaml_falls_back_to_none() {
        let doc = agent_doc(
            r#"name: coder
memory:
  top_k: not-a-number
"#,
            "body\n",
        );
        let a = AgentProfile::try_from(doc).unwrap();
        assert!(
            a.memory.is_none(),
            "invalid memory section must silently fall back"
        );
    }

    #[test]
    fn tools_with_invalid_refs_are_skipped() {
        let doc = agent_doc(
            "name: x\ntools:\n  - builtin:read\n  - nope:bad\n  - mcp:gh",
            "b",
        );
        let a = AgentProfile::try_from(doc).unwrap();
        assert_eq!(
            a.tools,
            vec![ToolRef::Builtin("read".into()), ToolRef::Mcp("gh".into())]
        );
    }

    #[test]
    fn skills_non_sequence_defaults_empty() {
        let doc = agent_doc("name: x\nskills: not-a-list", "b");
        let a = AgentProfile::try_from(doc).unwrap();
        assert!(a.skills.is_empty());
    }

    #[test]
    fn invalid_model_yaml_falls_back_to_none() {
        // model as a sequence cannot deserialize into ModelConfig → None
        let doc = agent_doc("name: x\nmodel:\n  - nope", "b");
        let a = AgentProfile::try_from(doc).unwrap();
        assert!(a.model.is_none());
    }

    #[test]
    fn invalid_permissions_yaml_falls_back_to_none() {
        let doc = agent_doc("name: x\npermissions: \"just a string\"", "b");
        let a = AgentProfile::try_from(doc).unwrap();
        assert!(a.permissions.is_none());
    }

    #[test]
    fn commands_with_invalid_entries_are_filtered() {
        let doc = agent_doc(
            "name: x\ncommands:\n  - name: ok\n    description: good\n  - just-a-string\n",
            "b",
        );
        let a = AgentProfile::try_from(doc).unwrap();
        assert_eq!(a.commands.len(), 1);
        assert_eq!(a.commands[0].name, "ok");
        assert_eq!(a.commands[0].description, "good");
    }

    #[test]
    fn multibyte_name_and_mode_preserved() {
        let doc = agent_doc("name: 中文代理\nmode: 计划", "b");
        let a = AgentProfile::try_from(doc).unwrap();
        assert_eq!(a.name, "中文代理");
        assert_eq!(a.mode.as_deref(), Some("计划"));
    }

    #[test]
    fn body_preserved_verbatim_with_newlines() {
        let doc = agent_doc("name: x", "line1\nline2\n\nline4");
        let a = AgentProfile::try_from(doc).unwrap();
        assert_eq!(a.body, "line1\nline2\n\nline4");
    }
}
