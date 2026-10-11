use serde::{Deserialize, Serialize};

/// Tool reference — serialized as the `"builtin:read"` string form in both
/// directions, matching the hand-written `Deserialize` (a derived newtype-variant
/// `Serialize` would emit `{"builtin":"read"}` and break the round-trip).
#[derive(Debug, Clone, PartialEq)]
pub enum ToolRef {
    Builtin(String),
    Script(String),
    Mcp(String),
}

impl std::str::FromStr for ToolRef {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.splitn(2, ':').collect();
        match parts.len() {
            2 => match parts[0] {
                "builtin" => Ok(ToolRef::Builtin(parts[1].to_string())),
                "script" => Ok(ToolRef::Script(parts[1].to_string())),
                "mcp" => Ok(ToolRef::Mcp(parts[1].to_string())),
                other => Err(format!("Unknown tool source: {other}")),
            },
            1 => Ok(ToolRef::Builtin(parts[0].to_string())),
            _ => Err(format!("Invalid tool reference format: {s}")),
        }
    }
}

impl serde::Serialize for ToolRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let s = match self {
            ToolRef::Builtin(x) => format!("builtin:{x}"),
            ToolRef::Script(x) => format!("script:{x}"),
            ToolRef::Mcp(x) => format!("mcp:{x}"),
        };
        serializer.serialize_str(&s)
    }
}

impl<'de> serde::Deserialize<'de> for ToolRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Model configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub default: String,
    /// List of global model registry IDs this agent can use.
    /// Empty vec means all registered models are available.
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub routing: Vec<RouteRule>,
    pub fallback: Option<String>,
}

/// Route rule
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRule {
    pub pattern: Vec<String>,
    pub prefer: String,
}

/// Permission action
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    Allow,
    #[default]
    Ask,
    Deny,
}

/// Permission rule
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    pub tool: Vec<String>,
    pub action: PermissionAction,
    #[serde(default)]
    pub allowed_commands: Option<Vec<String>>,
}

/// Permission configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionConfig {
    #[serde(default)]
    pub default: PermissionAction,
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
}

/// Hook trigger condition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookTrigger {
    pub matcher: Vec<String>,
    pub script: Option<String>,
}

/// Hook configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    #[serde(default, rename = "postToolResult")]
    pub post_tool_result: Vec<HookTrigger>,
    #[serde(default, rename = "preToolUse")]
    pub pre_tool_use: Vec<HookTrigger>,
}

/// Slash command
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
}

/// UI preference
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub color: Option<String>,
    pub greeting: Option<String>,
}

/// Skill trigger condition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillConditions {
    #[serde(default)]
    pub requires_tools: Vec<ToolRef>,
    #[serde(default)]
    pub match_file_suffix: Vec<String>,
    /// Keyword/regex triggers for auto-activation
    #[serde(default)]
    pub trigger: Option<SkillTrigger>,
    /// Lifecycle hooks (preToolUse, postToolResult)
    #[serde(default)]
    pub hooks: Option<HookConfig>,
}

/// Keyword or regex trigger for skill auto-activation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillTrigger {
    /// Match type: "keyword" or "regex"
    #[serde(default = "default_trigger_type")]
    pub trigger_type: String,
    /// Patterns to match against user input
    pub patterns: Vec<String>,
}

fn default_trigger_type() -> String {
    "keyword".into()
}

/// Coerce a YAML scalar (string, number, or bool) to its string form.
///
/// YAML parses an unquoted `version: 1.2` as a **number**, so `Value::as_str()`
/// returns `None` and the field would be silently dropped. This reconstructs a
/// string from numbers/bools too, so frontmatter that writes versions or
/// descriptions unquoted still round-trips. Sequences/mappings return `None`.
pub(crate) fn scalar_str(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Profile validation error (shared by Agent and Skill)
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("Missing required field: {0}")]
    MissingField(&'static str),
    #[error("Field '{0}' is invalid: {1}")]
    InvalidField(&'static str, String),
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ToolRef::from_str parser ──

    #[test]
    fn toolref_from_str_prefixed_variants() {
        assert_eq!(
            "builtin:read".parse::<ToolRef>().unwrap(),
            ToolRef::Builtin("read".into())
        );
        assert_eq!(
            "script:build.py".parse::<ToolRef>().unwrap(),
            ToolRef::Script("build.py".into())
        );
        assert_eq!(
            "mcp:github".parse::<ToolRef>().unwrap(),
            ToolRef::Mcp("github".into())
        );
    }

    #[test]
    fn toolref_bare_name_becomes_builtin() {
        assert_eq!(
            "read".parse::<ToolRef>().unwrap(),
            ToolRef::Builtin("read".into())
        );
        assert_eq!(
            "write".parse::<ToolRef>().unwrap(),
            ToolRef::Builtin("write".into())
        );
    }

    #[test]
    fn toolref_unknown_source_is_error() {
        assert!("foo:bar".parse::<ToolRef>().is_err());
        assert!(
            "Builtin:read".parse::<ToolRef>().is_err(),
            "source matching is case-sensitive"
        );
        assert!("MCP:gh".parse::<ToolRef>().is_err());
    }

    #[test]
    fn toolref_empty_and_colon_edges() {
        // splitn(2, ':') on "" yields [""] → len 1 → Builtin("")
        assert_eq!("".parse::<ToolRef>().unwrap(), ToolRef::Builtin("".into()));
        // "builtin:" → Builtin with empty target
        assert_eq!(
            "builtin:".parse::<ToolRef>().unwrap(),
            ToolRef::Builtin("".into())
        );
        // ":" → ["", ""], parts[0]="" is not a known source → Err
        assert!(":".parse::<ToolRef>().is_err());
    }

    #[test]
    fn toolref_keeps_colons_inside_target() {
        // splitn(2) splits only on the FIRST colon
        assert_eq!(
            "script:a:b:c".parse::<ToolRef>().unwrap(),
            ToolRef::Script("a:b:c".into())
        );
        assert_eq!(
            "mcp:host:1234/path".parse::<ToolRef>().unwrap(),
            ToolRef::Mcp("host:1234/path".into())
        );
    }

    #[test]
    fn toolref_multibyte_target_no_panic() {
        assert_eq!(
            "script:写脚本".parse::<ToolRef>().unwrap(),
            ToolRef::Script("写脚本".into())
        );
        assert_eq!(
            "builtin:中文工具".parse::<ToolRef>().unwrap(),
            ToolRef::Builtin("中文工具".into())
        );
    }

    #[test]
    fn toolref_error_messages_name_the_bad_input() {
        let err = "unknown:x".parse::<ToolRef>().unwrap_err();
        assert!(err.contains("unknown"), "got: {err}");
    }

    // ── ToolRef serde: both directions use the "builtin:read" string form ──
    // (a derived newtype-variant Serialize would emit {"builtin":"read"} and
    // break the round-trip, so Serialize is hand-written to match Deserialize).

    #[test]
    fn toolref_serializes_to_prefixed_string_form() {
        assert_eq!(
            serde_json::to_string(&ToolRef::Script("x.py".into())).unwrap(),
            "\"script:x.py\""
        );
        assert_eq!(
            serde_json::to_string(&ToolRef::Mcp("gh".into())).unwrap(),
            "\"mcp:gh\""
        );
        assert_eq!(
            serde_json::to_string(&ToolRef::Builtin("read".into())).unwrap(),
            "\"builtin:read\""
        );
    }

    #[test]
    fn toolref_serde_round_trip_is_symmetric() {
        for t in [
            ToolRef::Builtin("read".into()),
            ToolRef::Script("build.py".into()),
            ToolRef::Mcp("host:1234/x".into()),
        ] {
            let json = serde_json::to_string(&t).unwrap();
            let back: ToolRef = serde_json::from_str(&json).unwrap();
            assert_eq!(
                back, t,
                "round-trip must preserve the variant, json: {json}"
            );
        }
    }

    #[test]
    fn toolref_deserialize_accepts_prefixed_string() {
        let v: ToolRef = serde_json::from_str("\"mcp:gh\"").unwrap();
        assert_eq!(v, ToolRef::Mcp("gh".into()));
    }

    #[test]
    fn toolref_deserialize_rejects_unknown_source() {
        let r: Result<ToolRef, _> = serde_json::from_str("\"foo:bar\"");
        assert!(r.is_err());
    }

    #[test]
    fn toolref_yaml_roundtrip_through_skill_conditions() {
        // requires_tools items are custom-deserialized from "script:foo" strings
        let c: SkillConditions =
            serde_yaml::from_str("requires_tools:\n  - script:build.py\n  - builtin:read").unwrap();
        assert_eq!(
            c.requires_tools,
            vec![
                ToolRef::Script("build.py".into()),
                ToolRef::Builtin("read".into()),
            ]
        );
    }

    #[test]
    fn toolref_invalid_in_requires_tools_fails_parse() {
        let r: Result<SkillConditions, _> = serde_yaml::from_str("requires_tools:\n  - foo:bar");
        assert!(r.is_err(), "custom Deserialize surfaces parse errors");
    }

    // ── PermissionAction / PermissionConfig ──

    #[test]
    fn permission_action_defaults_to_ask() {
        assert_eq!(PermissionAction::default(), PermissionAction::Ask);
    }

    #[test]
    fn permission_action_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&PermissionAction::Allow).unwrap(),
            "\"allow\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionAction::Ask).unwrap(),
            "\"ask\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionAction::Deny).unwrap(),
            "\"deny\""
        );
        let v: PermissionAction = serde_json::from_str("\"ask\"").unwrap();
        assert_eq!(v, PermissionAction::Ask);
    }

    #[test]
    fn permission_action_unknown_variant_is_error() {
        assert!(serde_json::from_str::<PermissionAction>("\"maybe\"").is_err());
    }

    #[test]
    fn permission_config_empty_yaml_defaults_to_ask() {
        let cfg: PermissionConfig = serde_yaml::from_str("").unwrap();
        assert_eq!(cfg.default, PermissionAction::Ask);
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn permission_config_missing_fields_default() {
        let cfg: PermissionConfig = serde_yaml::from_str("default: deny").unwrap();
        assert_eq!(cfg.default, PermissionAction::Deny);
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn permission_rule_roundtrip() {
        let cfg: PermissionConfig = serde_yaml::from_str(
            "default: ask\nrules:\n  - tool: [bash]\n    action: allow\n    allowed_commands: [\"ls\", \"git status\"]",
        ).unwrap();
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].tool, vec!["bash".to_string()]);
        assert_eq!(cfg.rules[0].action, PermissionAction::Allow);
        assert_eq!(
            cfg.rules[0].allowed_commands.as_deref(),
            Some(&["ls".to_string(), "git status".to_string()][..])
        );
    }

    // ── SkillTrigger defaults ──

    #[test]
    fn skill_trigger_defaults_to_keyword_type() {
        let t: SkillTrigger = serde_yaml::from_str("patterns:\n  - fix").unwrap();
        assert_eq!(t.trigger_type, "keyword");
        assert_eq!(t.patterns, vec!["fix".to_string()]);
    }

    #[test]
    fn skill_trigger_explicit_regex_type() {
        let t: SkillTrigger =
            serde_yaml::from_str("trigger_type: regex\npatterns:\n  - \"foo.*\"").unwrap();
        assert_eq!(t.trigger_type, "regex");
    }

    // ── ModelConfig / RouteRule / HookConfig ──

    #[test]
    fn model_config_defaults_on_missing_fields() {
        let m: ModelConfig = serde_yaml::from_str("default: gpt4").unwrap();
        assert_eq!(m.default, "gpt4");
        assert!(m.models.is_empty());
        assert!(m.routing.is_empty());
        assert!(m.fallback.is_none());
    }

    #[test]
    fn model_config_parses_routing_rules() {
        let m: ModelConfig = serde_yaml::from_str(
            "default: a\nmodels:\n  - m1\n  - m2\nrouting:\n  - pattern: [测试, test]\n    prefer: ollama\nfallback: m2",
        ).unwrap();
        assert_eq!(m.models, vec!["m1".to_string(), "m2".to_string()]);
        assert_eq!(m.routing.len(), 1);
        assert_eq!(m.routing[0].prefer, "ollama");
        assert_eq!(m.fallback.as_deref(), Some("m2"));
    }

    #[test]
    fn hook_config_camel_case_renames() {
        let h: HookConfig =
            serde_yaml::from_str("postToolResult:\n  - matcher: [bash]\n    script: deny.sh")
                .unwrap();
        assert_eq!(h.post_tool_result.len(), 1);
        assert_eq!(h.post_tool_result[0].matcher, vec!["bash".to_string()]);
        assert_eq!(h.post_tool_result[0].script.as_deref(), Some("deny.sh"));
        assert!(
            h.pre_tool_use.is_empty(),
            "missing preToolUse defaults to empty"
        );
    }
}
