use starriver_skills::profile::parser::parse_document;
use starriver_skills::profile::skill::SkillProfile;

#[test]
fn test_skill_profile_minimal() {
    let content = "---\nname: code-review\n---\n\nReview code here.\n";
    let doc = parse_document(content).unwrap();
    let profile = SkillProfile::try_from(doc).unwrap();
    assert_eq!(profile.name, "code-review");
    assert!(!profile.disable_model_invocation);
    assert!(!profile.always_on);
}

#[test]
fn test_skill_profile_full() {
    let content = r#"---
name: code-review
version: 1.2.0
description: Review code in 5 dimensions
disable-model-invocation: false
user-invocable: true
command: /review
command-aliases:
  - /code-review
always-on: false
exclusive: false
conditions:
  requires_tools:
    - "builtin:read"
    - "builtin:grep"
  match_file_suffix:
    - ".rs"
    - ".ts"
tools:
  - builtin:read
  - builtin:grep
allowed-tools:
  - read
  - grep
depends-on:
  - git-basics
min-platform-version: ">=0.3.0"
---

## Review Dimensions
1. correctness
"#;
    let doc = parse_document(content).unwrap();
    let profile = SkillProfile::try_from(doc).unwrap();
    assert_eq!(profile.name, "code-review");
    assert_eq!(
        profile.description.as_deref(),
        Some("Review code in 5 dimensions")
    );
    assert_eq!(profile.command.as_deref(), Some("/review"));
    assert_eq!(profile.command_aliases, vec!["/code-review"]);
    assert!(!profile.disable_model_invocation);
    assert!(profile.user_invocable);
    assert!(profile.conditions.is_some());
    assert_eq!(profile.tools.len(), 2);
    assert!(profile.allowed_tools.is_some());
    assert_eq!(profile.depends_on, vec!["git-basics"]);
    assert!(profile.body.contains("Review Dimensions"));
}

#[test]
fn test_skill_profile_missing_name() {
    let content = "---\nversion: 1.0\n---\nbody";
    let doc = parse_document(content).unwrap();
    let result = SkillProfile::try_from(doc);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("name"));
}
