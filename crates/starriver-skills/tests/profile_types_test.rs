use starriver_skills::profile::types::{PermissionAction, ToolRef};

#[test]
fn test_tool_ref_parse_builtin() {
    let t: ToolRef = "builtin:read".parse().unwrap();
    assert_eq!(t, ToolRef::Builtin("read".into()));
}

#[test]
fn test_tool_ref_parse_script() {
    let t: ToolRef = "script:tools/custom.ts".parse().unwrap();
    assert_eq!(t, ToolRef::Script("tools/custom.ts".into()));
}

#[test]
fn test_tool_ref_parse_mcp() {
    let t: ToolRef = "mcp:github".parse().unwrap();
    assert_eq!(t, ToolRef::Mcp("github".into()));
}

#[test]
fn test_tool_ref_parse_bare_name() {
    let t: ToolRef = "read".parse().unwrap();
    assert_eq!(t, ToolRef::Builtin("read".into()));
}

#[test]
fn test_tool_ref_parse_invalid() {
    let result: Result<ToolRef, _> = "unknown:foo".parse();
    assert!(result.is_err());
}

#[test]
fn test_permission_action_default() {
    assert_eq!(PermissionAction::default(), PermissionAction::Ask);
}
