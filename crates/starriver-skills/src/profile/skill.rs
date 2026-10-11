use crate::profile::parser::RawDocument;
use crate::profile::types::*;
use std::collections::HashMap;

/// Skill configuration profile (SKILL.md)
#[derive(Debug, Clone)]
pub struct SkillProfile {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub disable_model_invocation: bool,
    pub user_invocable: bool,
    pub command: Option<String>,
    pub command_aliases: Vec<String>,
    pub always_on: bool,
    pub conditions: Option<SkillConditions>,
    pub tools: Vec<ToolRef>,
    pub allowed_tools: Option<Vec<String>>,
    pub depends_on: Vec<String>,
    pub exclusive: bool,
    pub hooks: Option<HookConfig>,
    pub min_platform_version: Option<String>,
    pub body: String,
}

impl TryFrom<RawDocument> for SkillProfile {
    type Error = ProfileError;

    fn try_from(doc: RawDocument) -> Result<Self, Self::Error> {
        let fm = &doc.frontmatter;

        let name = fm
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or(ProfileError::MissingField("name"))?;

        Ok(SkillProfile {
            name,
            version: string_val(fm, "version"),
            description: string_val(fm, "description"),
            disable_model_invocation: bool_val(fm, "disable-model-invocation"),
            user_invocable: bool_val(fm, "user-invocable"),
            command: string_val(fm, "command"),
            command_aliases: string_vec(fm, "command-aliases"),
            always_on: bool_val(fm, "always-on"),
            conditions: fm
                .get("conditions")
                .and_then(|v| serde_yaml::from_value(v.clone()).ok()),
            tools: parse_tools(fm),
            allowed_tools: parse_allowed_tools(fm),
            depends_on: string_vec(fm, "depends-on"),
            exclusive: bool_val(fm, "exclusive"),
            hooks: fm
                .get("hooks")
                .and_then(|v| serde_yaml::from_value(v.clone()).ok()),
            min_platform_version: string_val(fm, "min-platform-version"),
            body: doc.body,
        })
    }
}

// -- helpers --

fn bool_val(fm: &HashMap<String, serde_yaml::Value>, key: &str) -> bool {
    fm.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn string_val(fm: &HashMap<String, serde_yaml::Value>, key: &str) -> Option<String> {
    fm.get(key).and_then(scalar_str)
}

fn string_vec(fm: &HashMap<String, serde_yaml::Value>, key: &str) -> Vec<String> {
    fm.get(key)
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_tools(fm: &HashMap<String, serde_yaml::Value>) -> Vec<ToolRef> {
    fm.get("tools")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().and_then(|s| s.parse::<ToolRef>().ok()))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_allowed_tools(fm: &HashMap<String, serde_yaml::Value>) -> Option<Vec<String>> {
    let result: Vec<String> = fm
        .get("allowed-tools")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::parser::parse_document;

    fn skill_doc(yaml: &str, body: &str) -> RawDocument {
        parse_document(&format!("---\n{yaml}\n---\n{body}")).unwrap()
    }

    #[test]
    fn parses_full_skill_profile() {
        let doc = skill_doc(
            r#"name: code-review
version: "1.2"
description: Review code
command: review
command-aliases:
  - cr
  - rc
user-invocable: true
always-on: true
exclusive: true
min-platform-version: 0.5.0
depends-on:
  - git
allowed-tools:
  - read
  - glob
tools:
  - builtin:read
  - script:lint.sh
conditions:
  match_file_suffix:
    - .rs
hooks:
  preToolUse:
    - matcher: [bash]
      script: deny.sh
"#,
            "# body text\nline two",
        );
        let s = SkillProfile::try_from(doc).unwrap();
        assert_eq!(s.name, "code-review");
        assert_eq!(s.version.as_deref(), Some("1.2"));
        assert_eq!(s.description.as_deref(), Some("Review code"));
        assert_eq!(s.command.as_deref(), Some("review"));
        assert_eq!(s.command_aliases, vec!["cr", "rc"]);
        assert!(s.user_invocable && s.always_on && s.exclusive);
        assert_eq!(s.min_platform_version.as_deref(), Some("0.5.0"));
        assert_eq!(s.depends_on, vec!["git"]);
        assert_eq!(
            s.allowed_tools.as_deref(),
            Some(&["read".to_string(), "glob".to_string()][..])
        );
        assert_eq!(
            s.tools,
            vec![
                ToolRef::Builtin("read".into()),
                ToolRef::Script("lint.sh".into())
            ]
        );
        assert_eq!(
            s.conditions.unwrap().match_file_suffix,
            vec![".rs".to_string()]
        );
        assert_eq!(s.hooks.unwrap().pre_tool_use.len(), 1);
        assert_eq!(s.body, "# body text\nline two");
    }

    #[test]
    fn unquoted_numeric_version_is_preserved() {
        // YAML parses `version: 1.2` as a float; scalar_str converts it back.
        let doc = skill_doc("name: x\nversion: 1.2", "b");
        let s = SkillProfile::try_from(doc).unwrap();
        assert_eq!(s.version.as_deref(), Some("1.2"));
    }

    #[test]
    fn integer_version_is_preserved() {
        let doc = skill_doc("name: x\nversion: 3", "b");
        let s = SkillProfile::try_from(doc).unwrap();
        assert_eq!(s.version.as_deref(), Some("3"));
    }

    #[test]
    fn bool_description_is_coerced() {
        // `description: true` is a YAML bool → coerced to "true".
        let doc = skill_doc("name: x\ndescription: true", "b");
        let s = SkillProfile::try_from(doc).unwrap();
        assert_eq!(s.description.as_deref(), Some("true"));
    }

    #[test]
    fn missing_name_is_error() {
        let doc = skill_doc("version: 1.0", "body");
        let err = SkillProfile::try_from(doc).unwrap_err();
        assert!(
            matches!(err, ProfileError::MissingField("name")),
            "got: {err}"
        );
    }

    #[test]
    fn name_as_non_string_is_missing() {
        // name: 123 parses as a number → as_str() → None → MissingField
        let doc = skill_doc("name: 123", "body");
        let err = SkillProfile::try_from(doc).unwrap_err();
        assert!(matches!(err, ProfileError::MissingField("name")));
    }

    #[test]
    fn all_optional_fields_default() {
        let doc = skill_doc("name: bare", "body");
        let s = SkillProfile::try_from(doc).unwrap();
        assert_eq!(s.name, "bare");
        assert!(s.version.is_none());
        assert!(s.description.is_none());
        assert!(!s.disable_model_invocation);
        assert!(!s.user_invocable);
        assert!(s.command.is_none());
        assert!(s.command_aliases.is_empty());
        assert!(!s.always_on);
        assert!(s.conditions.is_none());
        assert!(s.tools.is_empty());
        assert!(s.allowed_tools.is_none());
        assert!(s.depends_on.is_empty());
        assert!(!s.exclusive);
        assert!(s.hooks.is_none());
        assert!(s.min_platform_version.is_none());
        assert_eq!(s.body, "body");
    }

    #[test]
    fn bool_fields_ignore_non_bool_values() {
        // YAML strings "yes"/"maybe" are not bools → as_bool() → None → false
        let doc = skill_doc("name: x\nuser-invocable: \"yes\"\nalways-on: maybe", "b");
        let s = SkillProfile::try_from(doc).unwrap();
        assert!(!s.user_invocable);
        assert!(!s.always_on);
    }

    #[test]
    fn tools_with_invalid_refs_are_skipped() {
        let doc = skill_doc(
            "name: x\ntools:\n  - builtin:read\n  - foo:bar\n  - mcp:gh",
            "b",
        );
        let s = SkillProfile::try_from(doc).unwrap();
        assert_eq!(
            s.tools,
            vec![ToolRef::Builtin("read".into()), ToolRef::Mcp("gh".into())]
        );
    }

    #[test]
    fn allowed_tools_empty_becomes_none() {
        let doc = skill_doc("name: x\nallowed-tools: []", "b");
        let s = SkillProfile::try_from(doc).unwrap();
        assert!(s.allowed_tools.is_none());
    }

    #[test]
    fn string_vec_non_sequence_becomes_empty() {
        // scalar (not a sequence) → as_sequence() → None → empty vec
        let doc = skill_doc("name: x\ndepends-on: git", "b");
        let s = SkillProfile::try_from(doc).unwrap();
        assert!(s.depends_on.is_empty());
    }

    #[test]
    fn invalid_hooks_yaml_falls_back_to_none() {
        let doc = skill_doc("name: x\nhooks: not-a-mapping", "b");
        let s = SkillProfile::try_from(doc).unwrap();
        assert!(s.hooks.is_none());
    }

    #[test]
    fn invalid_conditions_yaml_falls_back_to_none() {
        let doc = skill_doc("name: x\nconditions: [1, 2, 3]", "b");
        let s = SkillProfile::try_from(doc).unwrap();
        assert!(s.conditions.is_none());
    }

    #[test]
    fn multibyte_name_and_command_preserved() {
        let doc = skill_doc("name: 写代码助手\ncommand: 帮助", "b");
        let s = SkillProfile::try_from(doc).unwrap();
        assert_eq!(s.name, "写代码助手");
        assert_eq!(s.command.as_deref(), Some("帮助"));
    }
}
