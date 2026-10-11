//! Conversion functions from host types to protocol JSON shapes.
//!
//! The `#[api_method(..., result = "path::to::fn")]` attribute requires the
//! conversion functions to be visible in the host crate (where the macro
//! expands), so the protocol shape builders are centralized here; the
//! hand-written ones in protocol-server are gradually being updated to
//! reference these, to avoid the two JSON shapes drifting apart.

use serde_json::{json, Value};

use crate::types::SkillInfo;

/// `SkillInfo` → protocol `skills.*` result entry (camelCase, matching the client contract).
pub fn skill_info_json(s: &SkillInfo) -> Value {
    json!({
        "name": s.name,
        "description": s.description,
        "hasCommand": s.has_command,
        "command": s.command,
        "version": s.version,
        "alwaysOn": s.always_on,
        "hasKeyword": s.has_keyword,
        "hasSuffix": s.has_suffix,
        "hasHooks": s.has_hooks,
        "enabled": s.enabled,
        "body": s.body,
    })
}

/// `Vec<SkillInfo>` → protocol result array (result conversion for `skills.delete` and similar methods).
pub fn skill_info_list(skills: Vec<SkillInfo>) -> Value {
    Value::Array(skills.iter().map(skill_info_json).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SkillInfo {
        SkillInfo {
            name: "review".into(),
            description: "run a code review".into(),
            has_command: true,
            command: Some("scripts/review.sh".into()),
            version: Some("1.2.0".into()),
            always_on: true,
            has_keyword: true,
            has_suffix: false,
            has_hooks: true,
            enabled: true,
            body: "---\nname: review\n---\nbody".into(),
        }
    }

    #[test]
    fn skill_info_json_uses_camel_case_keys() {
        let value = skill_info_json(&sample());
        assert_eq!(value["name"], "review");
        assert_eq!(value["description"], "run a code review");
        assert_eq!(value["hasCommand"], true);
        assert_eq!(value["command"], "scripts/review.sh");
        assert_eq!(value["version"], "1.2.0");
        assert_eq!(value["alwaysOn"], true);
        assert_eq!(value["hasKeyword"], true);
        assert_eq!(value["hasSuffix"], false);
        assert_eq!(value["hasHooks"], true);
        assert_eq!(value["enabled"], true);
        assert_eq!(value["body"], "---\nname: review\n---\nbody");
        // snake_case keys must never appear (the protocol client contract is camelCase).
        assert!(value.get("has_command").is_none());
        assert!(value.get("always_on").is_none());
    }

    #[test]
    fn skill_info_json_handles_absent_optional_fields() {
        let mut skill = sample();
        skill.command = None;
        skill.version = None;
        let value = skill_info_json(&skill);
        assert!(value["command"].is_null());
        assert!(value["version"].is_null());
    }

    #[test]
    fn skill_info_list_produces_array_in_order() {
        let mut a = sample();
        a.name = "alpha".into();
        let mut b = sample();
        b.name = "beta".into();
        let value = skill_info_list(vec![a, b]);
        assert!(value.is_array());
        assert_eq!(value.as_array().unwrap().len(), 2);
        assert_eq!(value[0]["name"], "alpha");
        assert_eq!(value[1]["name"], "beta");
    }

    #[test]
    fn empty_list_is_empty_array() {
        assert_eq!(skill_info_list(Vec::new()), Value::Array(vec![]));
    }
}
