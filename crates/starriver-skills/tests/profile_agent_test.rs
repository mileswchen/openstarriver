use starriver_skills::profile::agent::AgentProfile;
use starriver_skills::profile::parser::parse_document;

#[test]
fn test_agent_profile_from_minimal_doc() {
    let content = "---\nname: test-agent\n---\n\nHello, I am an agent.\n";
    let doc = parse_document(content).unwrap();
    let profile = AgentProfile::try_from(doc).unwrap();
    assert_eq!(profile.name, "test-agent");
    assert!(profile.skills.is_empty());
    assert!(profile.tools.is_empty());
    assert_eq!(profile.body.trim(), "Hello, I am an agent.");
}

#[test]
fn test_agent_profile_with_tools_and_skills() {
    let content = r#"---
name: coding-agent
version: 1.0.0
skills:
  - code-review
  - unit-test
tools:
  - builtin:read
  - builtin:write
  - mcp:github
model:
  default: deepseek-v4-pro
  fallback: ollama:qwen3:4b
permissions:
  default: ask
  rules:
    - tool: [read, grep]
      action: allow
commands:
  - name: plan
    description: Plan mode
---

You are a coding agent.
"#;
    let doc = parse_document(content).unwrap();
    let profile = AgentProfile::try_from(doc).unwrap();
    assert_eq!(profile.name, "coding-agent");
    assert_eq!(profile.version.as_deref(), Some("1.0.0"));
    assert_eq!(profile.skills.len(), 2);
    assert_eq!(profile.tools.len(), 3);
    assert!(profile.model.is_some());
    assert!(profile.permissions.is_some());
    assert_eq!(profile.commands.len(), 1);
    assert_eq!(profile.commands[0].name, "plan");
}

#[test]
fn test_agent_profile_parses_mode_field() {
    let content = "---\nname: coding-agent\nmode: build\ntools: [read, grep]\n---\n\nbody";
    let doc = parse_document(content).unwrap();
    let profile = AgentProfile::try_from(doc).unwrap();
    assert_eq!(profile.mode.as_deref(), Some("build"));
}

#[test]
fn test_agent_profile_mode_defaults_to_none() {
    // Profiles without a `mode:` field stay AUTO (no default mode).
    let content = "---\nname: plain-chat\n---\n\nbody";
    let doc = parse_document(content).unwrap();
    let profile = AgentProfile::try_from(doc).unwrap();
    assert_eq!(profile.mode, None);
}

#[test]
fn test_agent_profile_missing_name() {
    let content = "---\nversion: 1.0\n---\nbody";
    let doc = parse_document(content).unwrap();
    let result = AgentProfile::try_from(doc);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("name"));
}
