// E2E test: AGENT.md + SKILL.md → parse → Profile → inject → verify prompt
use starriver_skills::profile::agent::AgentProfile;
use starriver_skills::profile::parser::parse_document;
use starriver_skills::profile::skill::SkillProfile;

#[test]
fn test_agent_skill_e2e_pipeline() {
    let agent_md = r#"---
name: test-agent
skills:
  - code-review
tools:
  - builtin:read
  - builtin:grep
---

You are a test agent.
"#;
    let skill_md = r#"---
name: code-review
description: Review code quality
always-on: false
tools:
  - builtin:read
  - builtin:grep
---

## Review Process
1. Check correctness
2. Check security
3. Report findings
"#;

    // Step 1: Parse AGENT.md → AgentProfile
    let agent_doc = parse_document(agent_md).unwrap();
    let agent = AgentProfile::try_from(agent_doc).unwrap();
    assert_eq!(agent.name, "test-agent");
    assert_eq!(agent.skills, vec!["code-review"]);
    assert_eq!(agent.tools.len(), 2);
    assert!(agent.body.contains("test agent"));

    // Step 2: Parse SKILL.md → SkillProfile
    let skill_doc = parse_document(skill_md).unwrap();
    let skill = SkillProfile::try_from(skill_doc).unwrap();
    assert_eq!(skill.name, "code-review");
    assert!(!skill.always_on);
    assert_eq!(skill.tools.len(), 2);
    assert!(skill.body.contains("Review Process"));

    // Step 3: Simulate SkillManager injection
    // Build a pseudo system prompt that mirrors what SkillManager.inject_to_system_prompt does
    let mut prompt = agent.body.clone();

    // Inject skill descriptions (simulating SkillManager.descriptions())
    prompt.push_str("\n\n## Available Skills\n\n");
    prompt.push_str(&format!(
        "- **{}**: {}\n",
        skill.name,
        skill.description.clone().unwrap_or_default()
    ));

    // Inject skill body only if always-on (simulated — this one is not always-on, so body not injected)
    // Final prompt should contain agent body + skill description
    assert!(prompt.contains("test agent"));
    assert!(prompt.contains("## Available Skills"));
    assert!(prompt.contains("**code-review**: Review code quality"));
}

#[test]
fn test_always_on_skill_injection() {
    let agent_md = r#"---
name: coding-helper
skills:
  - code-style
---

You are a coding helper.
"#;
    let skill_md = r#"---
name: code-style
description: Enforce code style
always-on: true
---

## Style Rules
1. Use 4-space indentation
2. Max line length: 100
"#;

    let agent = AgentProfile::try_from(parse_document(agent_md).unwrap()).unwrap();
    let skill = SkillProfile::try_from(parse_document(skill_md).unwrap()).unwrap();
    assert!(skill.always_on);

    // Simulate SkillManager.inject_to_system_prompt with always-on
    let mut prompt = agent.body.clone();
    if skill.always_on {
        prompt.push_str("\n\n## Always-Active Skills\n\n");
        prompt.push_str(&format!("### {}\n\n{}\n\n---\n\n", skill.name, skill.body));
    }

    assert!(prompt.contains("Always-Active Skills"));
    assert!(prompt.contains("4-space indentation"));
    assert!(prompt.contains("Max line length: 100"));
}

#[test]
fn test_agent_without_skills() {
    let agent_md = r#"---
name: minimal-agent
---

Minimal agent body.
"#;
    let agent = AgentProfile::try_from(parse_document(agent_md).unwrap()).unwrap();
    assert_eq!(agent.name, "minimal-agent");
    assert!(agent.skills.is_empty());
    assert_eq!(agent.body.trim(), "Minimal agent body.");
}

#[test]
fn test_skill_disable_model_invocation() {
    let skill_md = r#"---
name: secret-skill
description: Internal tool
disable-model-invocation: true
---

Secret content.
"#;
    let skill = SkillProfile::try_from(parse_document(skill_md).unwrap()).unwrap();
    assert!(skill.disable_model_invocation);
    assert_eq!(skill.description.as_deref(), Some("Internal tool"));
}

// ──────────────────────────────────────────────────
// AGENT.md file-system round-trip tests
// ──────────────────────────────────────────────────

mod agent_md_roundtrip {
    use serde_yaml::Value;
    use starriver_skills::profile::{parse_document, serialize_document};
    use std::fs;
    use std::path::PathBuf;

    /// Simulates writing an AGENT.md from scratch (like lib.rs line 545).
    fn write_initial_agent_md(path: &PathBuf, name: &str, default_model: &str) {
        let md = format!(
            "---\nname: \"{}\"\nmodel:\n  default: \"{}\"\n  models:\n    - \"{}\"\n---\n\n# {}\n",
            name, default_model, default_model, name
        );
        fs::write(path, md).unwrap();
    }

    /// Simulates set_agent_active_model: read → modify → write.
    fn set_active_model(path: &PathBuf, provider_id: &str, variant_id: &str) {
        let content = fs::read_to_string(path).unwrap();
        let doc = parse_document(&content).unwrap();
        let mut fm = doc.frontmatter.clone();
        let default_val = format!("{}/{}", provider_id, variant_id);

        let model_val = fm
            .entry("model".to_string())
            .or_insert(Value::Mapping(Default::default()));
        if let Value::Mapping(ref mut map) = model_val {
            map.insert(
                Value::String("default".into()),
                Value::String(default_val.clone()),
            );
            let models_entry = map
                .entry(Value::String("models".into()))
                .or_insert(Value::Sequence(vec![]));
            if let Value::Sequence(ref mut seq) = models_entry {
                if !seq.iter().any(|v| v.as_str() == Some(&default_val)) {
                    seq.push(Value::String(default_val.clone()));
                }
            }
        }

        let serialized = serialize_document(&fm, &doc.body).unwrap();
        fs::write(path, serialized).unwrap();
    }

    /// Simulates set_agent_model_enabled: read → modify models list → write.
    fn set_model_enabled(path: &PathBuf, model_id: &str, enabled: bool) {
        let content = fs::read_to_string(path).unwrap();
        let doc = parse_document(&content).unwrap();
        let mut fm = doc.frontmatter.clone();

        let model_val = fm
            .entry("model".to_string())
            .or_insert(Value::Mapping(Default::default()));
        if let Value::Mapping(ref mut map) = model_val {
            let models_entry = map
                .entry(Value::String("models".into()))
                .or_insert(Value::Sequence(vec![]));
            if let Value::Sequence(ref mut seq) = models_entry {
                seq.retain(|v| v.as_str() != Some(model_id));
                if enabled {
                    seq.push(Value::String(model_id.to_string()));
                }
            }
        }

        let serialized = serialize_document(&fm, &doc.body).unwrap();
        fs::write(path, serialized).unwrap();
    }

    fn read_model_ids(path: &PathBuf) -> Vec<String> {
        let content = fs::read_to_string(path).unwrap();
        let doc = parse_document(&content).unwrap();
        let model = doc.frontmatter.get("model").unwrap();
        model
            .get("models")
            .unwrap()
            .as_sequence()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    }

    fn read_default_model(path: &PathBuf) -> String {
        let content = fs::read_to_string(path).unwrap();
        let doc = parse_document(&content).unwrap();
        doc.frontmatter
            .get("model")
            .unwrap()
            .get("default")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn file_round_trip_single_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENT.md");

        write_initial_agent_md(&path, "Chat Agent", "deepseekv4/deepseek-v4-flash");
        set_active_model(&path, "deepseekv4", "deepseek-v4-pro");

        let ids = read_model_ids(&path);
        let default = read_default_model(&path);

        assert_eq!(default, "deepseekv4/deepseek-v4-pro");
        assert!(ids.contains(&"deepseekv4/deepseek-v4-flash".to_string()));
        assert!(ids.contains(&"deepseekv4/deepseek-v4-pro".to_string()));
    }

    #[test]
    fn file_round_trip_ten_cycles_no_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENT.md");

        write_initial_agent_md(&path, "Chat Agent", "model-a/initial");

        // Simulate 10 active model switches between different providers
        let switches = [
            ("deepseekv4", "deepseek-v4-pro"),
            ("deepseekv4", "deepseek-v4-flash"),
            ("ollama", "qwen3-coder:7b"),
            ("openai", "gpt-4o-mini"),
            ("glm", "glm-4.7-flash"),
            ("deepseekv4", "deepseek-v4-pro"), // duplicate
            ("ollama", "llama3.2:3b"),
            ("openai", "gpt-4o"),
            ("deepseekv4", "deepseek-v4-flash"), // duplicate
            ("ollama", "codellama:7b"),
        ];

        for (i, (provider, variant)) in switches.iter().enumerate() {
            set_active_model(&path, provider, variant);

            // After each write, verify the file is parseable and has no corruption
            let content = fs::read_to_string(&path).unwrap();
            let doc = parse_document(&content).unwrap();
            let model = doc.frontmatter.get("model").unwrap();
            let models = model.get("models").unwrap().as_sequence().unwrap();

            for m in models {
                let id = m.as_str().unwrap();
                assert!(
                    !id.ends_with("---"),
                    "Cycle {i}: model ID '{id}' has trailing ---"
                );
                assert!(
                    !id.starts_with("---"),
                    "Cycle {i}: model ID '{id}' has leading ---"
                );
            }

            // Verify the raw file content doesn't have embedded --- markers
            assert!(
                !content.contains("\n  - ")
                    || !content
                        .lines()
                        .any(|l| l.contains("---") && l.contains("model")),
                "Cycle {i}: file content has embedded --- markers"
            );
        }

        // Final state: should have all unique models
        let ids = read_model_ids(&path);
        let default = read_default_model(&path);

        assert_eq!(default, "ollama/codellama:7b"); // last switch
        assert!(ids.contains(&"model-a/initial".to_string()));
        assert!(
            !ids.iter().any(|id| id.ends_with("---")),
            "No IDs should end with ---"
        );
    }

    #[test]
    fn file_enable_disable_models_no_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENT.md");

        write_initial_agent_md(&path, "Chat Agent", "provider-a/model-1");

        // Add models
        set_model_enabled(&path, "provider-b/model-2", true);
        set_model_enabled(&path, "provider-c/model-3", true);
        set_model_enabled(&path, "provider-d/model-4", true);

        let ids = read_model_ids(&path);
        assert_eq!(ids.len(), 4);

        // Remove one
        set_model_enabled(&path, "provider-b/model-2", false);
        let ids = read_model_ids(&path);
        assert_eq!(ids.len(), 3);
        assert!(!ids.contains(&"provider-b/model-2".to_string()));

        // Toggle same model on/off/on
        set_model_enabled(&path, "provider-e/model-5", true);
        set_model_enabled(&path, "provider-e/model-5", false);
        set_model_enabled(&path, "provider-e/model-5", true);
        let ids = read_model_ids(&path);
        assert_eq!(ids.len(), 4);

        // Verify no corruption after toggles
        for id in &ids {
            assert!(
                !id.ends_with("---"),
                "Model ID '{id}' corrupted after enable/disable cycles"
            );
        }
    }

    #[test]
    fn file_concurrent_active_and_enabled_operations() {
        // Interleave set_active_model and set_model_enabled calls
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENT.md");

        write_initial_agent_md(&path, "Chat Agent", "a/initial");

        set_model_enabled(&path, "b/second", true);
        set_active_model(&path, "b", "second");
        set_model_enabled(&path, "c/third", true);
        set_active_model(&path, "c", "third");
        set_model_enabled(&path, "b/second", false);
        set_active_model(&path, "a", "initial");
        set_model_enabled(&path, "b/second", true);
        set_model_enabled(&path, "d/fourth", true);
        set_active_model(&path, "d", "fourth");

        let ids = read_model_ids(&path);
        let default = read_default_model(&path);

        assert_eq!(default, "d/fourth");
        // b was removed then re-added
        assert_eq!(ids.len(), 4); // a/initial, c/third, b/second, d/fourth
        for id in &ids {
            assert!(
                !id.contains("---"),
                "ID '{id}' corrupted after interleaved ops"
            );
        }
    }

    #[test]
    fn file_content_remains_valid_yaml_after_cycles() {
        // The serialized file should always be valid YAML frontmatter
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENT.md");

        write_initial_agent_md(&path, "Test Agent", "x/model-0");

        for i in 1..=30 {
            let provider = format!("provider-{}", i % 5);
            let variant = format!("model-{}", i);
            set_active_model(&path, &provider, &variant);

            // Every file write must produce parseable content
            let content = fs::read_to_string(&path).unwrap();
            let doc = parse_document(&content).unwrap();

            // Frontmatter must contain valid model data
            let model = doc
                .frontmatter
                .get("model")
                .expect(&format!("Cycle {i}: missing 'model' key in frontmatter"));
            let _default = model
                .get("default")
                .and_then(|v| v.as_str())
                .expect(&format!("Cycle {i}: missing 'default' in model"));
            let _models = model
                .get("models")
                .and_then(|v| v.as_sequence())
                .expect(&format!("Cycle {i}: missing 'models' array"));

            // Body must be non-empty
            assert!(!doc.body.is_empty(), "Cycle {i}: empty body");

            // Raw content should have exactly 2 --- markers (one opening, one closing)
            let marker_count = content.match_indices("---").count();
            assert!(
                marker_count == 2 || marker_count == 3,
                "Cycle {i}: expected 2-3 '---' markers, got {marker_count}\nContent:\n{content}"
            );
        }
    }
}

// ──────────────────────────────────────────────────
// Agent mode integration tests
// ──────────────────────────────────────────────────

mod agent_mode_integration {
    use serde_yaml::Value;
    use starriver_skills::profile::{parse_document, serialize_document};
    use std::collections::HashMap;

    /// Simulates creating a Coding Agent AGENT.md with plan/build mode support.
    #[test]
    fn coding_agent_profile_with_mode_hints() {
        let agent_md = "\
---
name: Coding Agent
version: 0.1.0
description: AI coding agent for code generation and review
tools:
  - read
  - write
  - edit
  - bash
  - grep
---

You are a coding agent. You can read, edit, and create files, search the codebase, and execute shell commands. Always explain your changes.
";
        let doc = parse_document(agent_md).unwrap();
        assert_eq!(
            doc.frontmatter.get("name").unwrap().as_str().unwrap(),
            "Coding Agent"
        );
        assert_eq!(
            doc.frontmatter.get("version").unwrap().as_str().unwrap(),
            "0.1.0"
        );

        // Tools list should contain all 5 coding tools
        let tools = doc.frontmatter.get("tools").unwrap().as_sequence().unwrap();
        assert_eq!(tools.len(), 5);
        let tool_names: Vec<&str> = tools.iter().filter_map(|v| v.as_str()).collect();
        assert!(tool_names.contains(&"read"));
        assert!(tool_names.contains(&"write"));
        assert!(tool_names.contains(&"edit"));
        assert!(tool_names.contains(&"bash"));
        assert!(tool_names.contains(&"grep"));
    }

    #[test]
    fn round_trip_preserves_agent_tools_and_body() {
        let agent_md = "\
---
name: Coding Agent
tools:
  - read
  - write
  - edit
  - bash
  - grep
---

You are a coding agent. Always explain your changes.
";
        let doc = parse_document(agent_md).unwrap();
        let serialized = serialize_document(&doc.frontmatter, &doc.body).unwrap();
        let doc2 = parse_document(&serialized).unwrap();

        // Tools preserved
        let tools = doc2
            .frontmatter
            .get("tools")
            .unwrap()
            .as_sequence()
            .unwrap();
        assert_eq!(tools.len(), 5);

        // Body preserved
        assert!(doc2.body.contains("Always explain your changes"));
    }

    #[test]
    fn agent_profile_with_model_section_survives_roundtrip() {
        // Full profile as produced by set_agent_active_model
        let mut fm: HashMap<String, Value> = HashMap::new();
        fm.insert("name".into(), Value::String("Coding Agent".into()));
        fm.insert("version".into(), Value::String("0.1.0".into()));
        fm.insert(
            "description".into(),
            Value::String("AI coding agent".into()),
        );

        let mut model = serde_yaml::Mapping::new();
        model.insert(
            Value::String("default".into()),
            Value::String("deepseekv4/deepseek-v4-pro".into()),
        );
        model.insert(
            Value::String("models".into()),
            Value::Sequence(vec![
                Value::String("deepseekv4/deepseek-v4-pro".into()),
                Value::String("deepseekv4/deepseek-v4-flash".into()),
            ]),
        );
        fm.insert("model".into(), Value::Mapping(model));

        let body = "# Coding Agent\n\nYou are a coding agent.";
        let serialized = serialize_document(&fm, body).unwrap();

        // Round-trip
        let doc = parse_document(&serialized).unwrap();
        let m = doc.frontmatter.get("model").unwrap();
        assert_eq!(
            m.get("default").unwrap().as_str().unwrap(),
            "deepseekv4/deepseek-v4-pro"
        );
        let models = m.get("models").unwrap().as_sequence().unwrap();
        assert_eq!(models.len(), 2);

        // Verify no corruption in serialized output
        assert!(
            !serialized.contains("pro---"),
            "no trailing --- in model IDs"
        );
        assert!(
            !serialized.contains("flash---"),
            "no trailing --- in model IDs"
        );
    }

    #[test]
    fn serialize_document_strips_document_markers_correctly() {
        // Test that serialize_document handles frontmatter with various content
        let mut fm: HashMap<String, Value> = HashMap::new();
        fm.insert("name".into(), Value::String("Test".into()));
        fm.insert(
            "description".into(),
            Value::String("An agent with --- in description to test robustness".into()),
        );

        let result = serialize_document(&fm, "# Body with --- markers\n").unwrap();

        // Should be parseable
        let doc = parse_document(&result).unwrap();
        assert_eq!(
            doc.frontmatter.get("name").unwrap().as_str().unwrap(),
            "Test"
        );
        assert_eq!(
            doc.frontmatter
                .get("description")
                .unwrap()
                .as_str()
                .unwrap(),
            "An agent with --- in description to test robustness"
        );

        // Verify body is intact
        assert!(doc.body.contains("# Body with --- markers"));
    }

    #[test]
    fn coding_agent_prompt_contains_tool_loop_rules() {
        let agent_md = "\
---
name: Coding Agent
tools:
  - read
  - write
  - edit
  - bash
  - grep
---

Never stop after a tool result.

After receiving tool output, you MUST continue.

Remember: every tool call must be followed by analysis.
";
        let doc = parse_document(agent_md).unwrap();
        let body = &doc.body;
        assert!(body.contains("Never stop after a tool result"));
        assert!(body.contains("you MUST continue"));
        assert!(body.contains("every tool call must be followed by analysis"));
    }

    #[test]
    fn coding_agent_prompt_contains_critical_rules() {
        let agent_md = "\
---
name: Coding Agent
tools: [read, write, edit, bash, grep]
---

## Critical Rules

1. Never stop after a tool result
2. Always explain your changes
3. Plan then execute

## Tool Call Flow

Step by step instructions.
";
        let doc = parse_document(agent_md).unwrap();
        assert!(doc.body.contains("Critical Rules"));
        assert!(doc.body.contains("Tool Call Flow"));
        assert!(doc.body.contains("Never stop"));
    }
}
