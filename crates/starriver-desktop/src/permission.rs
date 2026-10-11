#![allow(dead_code, unused)]
#[allow(dead_code)]
use starriver_kernel::config::PermissionRule;
use regex_lite::Regex;

/// Permission check result
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PermissionResult {
    pub allowed: bool,
    pub needs_approval: bool,
    pub reason: Option<String>,
}

/// Permission engine — rule-based matching with path/pattern support
#[allow(dead_code)]
pub struct PermissionEngine {
    rules: Vec<PermissionRule>,
}

impl PermissionEngine {
    pub fn new(rules: Vec<PermissionRule>) -> Self {
        Self { rules }
    }

    pub fn defaults() -> Self {
        Self {
            rules: vec![
                PermissionRule { tool: "read".into(), pattern: None, path: None, action: "allow".into() },
                PermissionRule { tool: "grep".into(), pattern: None, path: None, action: "allow".into() },
                PermissionRule { tool: "write".into(), pattern: None, path: None, action: "ask".into() },
                PermissionRule { tool: "edit".into(), pattern: None, path: None, action: "ask".into() },
                PermissionRule { tool: "bash".into(), pattern: None, path: None, action: "ask".into() },
                PermissionRule { tool: "git".into(), pattern: None, path: None, action: "ask".into() },
            ],
        }
    }

    /// Check tool permission with path/pattern matching.
    /// Path-specific rules take priority over generic tool rules.
    pub fn check(&self, tool: &str, path: &str, _cwd: &str) -> PermissionResult {
        // First pass: path-specific rules (higher priority)
        for rule in &self.rules {
            if rule.tool != tool { continue; }
            if let Some(ref rule_path) = rule.path {
                if path.starts_with(rule_path) {
                    return Self::action_result(&rule.action, tool);
                }
            }
            if let Some(ref pattern) = rule.pattern {
                if Self::match_pattern(pattern, path) {
                    return Self::action_result(&rule.action, tool);
                }
            }
        }
        // Second pass: generic tool rules (no path/pattern)
        for rule in &self.rules {
            if rule.tool == tool && rule.path.is_none() && rule.pattern.is_none() {
                return Self::action_result(&rule.action, tool);
            }
        }
        PermissionResult { allowed: true, needs_approval: false, reason: None }
    }


    /// Check if a pattern matches a path. Supports:
    /// - `glob:*.rs` — glob pattern (converted to regex internally)
    /// - anything else — treated as regex directly
    fn match_pattern(pattern: &str, path: &str) -> bool {
        if let Some(glob) = pattern.strip_prefix("glob:") {
            // Convert glob to regex
            let regex = Self::glob_to_regex(glob);
            regex_lite::Regex::new(&regex).map(|re| re.is_match(path)).unwrap_or(false)
        } else {
            regex_lite::Regex::new(pattern).map(|re| re.is_match(path)).unwrap_or(false)
        }
    }

    /// Convert a glob pattern to a regex string.
    /// Supports: * (non-slash), ** (any), ? (single non-slash), [abc] (char class)
    fn glob_to_regex(glob: &str) -> String {
        let mut result = String::from("^");
        let chars: Vec<char> = glob.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                    result.push_str(".*");
                    i += 2;
                    continue;
                }
                '*' => {
                    result.push_str("[^/]*");
                }
                '?' => {
                    result.push_str("[^/]");
                }
                '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
                    result.push('\\');
                    result.push(chars[i]);
                }
                _ => {
                    result.push(chars[i]);
                }
            }
            i += 1;
        }
        result.push('$');
        result
    }

    fn action_result(action: &str, tool: &str) -> PermissionResult {
        match action {
            "allow" => PermissionResult { allowed: true, needs_approval: false, reason: None },
            "deny" => PermissionResult { allowed: false, needs_approval: false,
                reason: Some(format!("Tool {tool} denied by rule")) },
            "ask" => PermissionResult { allowed: true, needs_approval: true,
                reason: Some(format!("Tool {tool} requires confirmation")) },
            _ => PermissionResult { allowed: true, needs_approval: false, reason: None },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::config::PermissionRule;

    #[test]
    fn test_permission_defaults_allow_read() {
        let engine = PermissionEngine::defaults();
        let result = engine.check("read", "/some/file.txt", "/home/user");
        assert!(result.allowed);
        assert!(!result.needs_approval);
    }

    #[test]
    fn test_permission_defaults_ask_write() {
        let engine = PermissionEngine::defaults();
        let result = engine.check("write", "/some/file.txt", "/home/user");
        assert!(result.allowed);
        assert!(result.needs_approval);
    }

    #[test]
    fn test_permission_empty_rules_allow() {
        let engine = PermissionEngine::new(vec![]);
        let result = engine.check("bash", "/any/path", "/cwd");
        assert!(result.allowed);
        assert!(!result.needs_approval);
    }

    #[test]
    fn test_permission_deny_rule() {
        let engine = PermissionEngine::new(vec![
            PermissionRule { tool: "dangerous".into(), pattern: None, path: None, action: "deny".into() },
        ]);
        let result = engine.check("dangerous", "/path", "/cwd");
        assert!(!result.allowed);
        assert!(result.reason.is_some());
        assert!(result.reason.unwrap().contains("denied"));
    }

    #[test]
    fn test_permission_unknown_tool_allowed() {
        let engine = PermissionEngine::new(vec![
            PermissionRule { tool: "read".into(), pattern: None, path: None, action: "allow".into() },
        ]);
        let result = engine.check("unknown-tool", "/path", "/cwd");
        assert!(result.allowed);
        assert!(!result.needs_approval);
    }

    #[test]
    fn test_permission_defaults_ask_bash() {
        let engine = PermissionEngine::defaults();
        let result = engine.check("bash", "/tmp/script.sh", "/cwd");
        assert!(result.allowed);
        assert!(result.needs_approval);
        let reason = result.reason.as_deref().unwrap();
        assert!(reason.contains("bash"));
    }

    #[test]
    fn test_permission_path_match_denies_specific_path() {
        let engine = PermissionEngine::new(vec![
            PermissionRule { tool: "write".into(), pattern: None, path: Some("/etc".into()), action: "deny".into() },
            PermissionRule { tool: "write".into(), pattern: None, path: None, action: "allow".into() },
        ]);
        let result = engine.check("write", "/etc/hosts", "/cwd");
        assert!(!result.allowed);
        let result = engine.check("write", "/home/user/file.txt", "/cwd");
        assert!(result.allowed);
    }

    #[test]
    fn test_permission_pattern_match_denies_sensitive_files() {
        let engine = PermissionEngine::new(vec![
            PermissionRule { tool: "read".into(), pattern: Some(r"\.(pem|key|env)$".into()), path: None, action: "deny".into() },
            PermissionRule { tool: "read".into(), pattern: None, path: None, action: "allow".into() },
        ]);
        let result = engine.check("read", "/home/user/.ssh/id_rsa.pem", "/cwd");
        assert!(!result.allowed);
        let result = engine.check("read", "/home/user/src/main.rs", "/cwd");
        assert!(result.allowed);
    }

    #[test]
    fn test_permission_glob_pattern_matches_files() {
        let engine = PermissionEngine::new(vec![
            PermissionRule {
                tool: "read".into(),
                pattern: Some("glob:**/*.pem".into()),
                path: None,
                action: "deny".into(),
            },
            PermissionRule {
                tool: "read".into(),
                pattern: None,
                path: None,
                action: "allow".into(),
            },
        ]);
        // Glob **/*.pem should match any .pem file
        assert!(!engine.check("read", "/home/user/.ssh/key.pem", "/cwd").allowed);
        assert!(!engine.check("read", "/etc/ssl/cert.pem", "/cwd").allowed);
        // Non-.pem files should be allowed
        assert!(engine.check("read", "/home/user/main.rs", "/cwd").allowed);
    }

    #[test]
    fn test_permission_glob_single_star() {
        let engine = PermissionEngine::new(vec![
            PermissionRule {
                tool: "read".into(),
                pattern: Some("glob:**/*.env".into()),
                path: None,
                action: "deny".into(),
            },
            PermissionRule {
                tool: "read".into(),
                pattern: None,
                path: None,
                action: "allow".into(),
            },
        ]);
        // **/*.env should match .env files in any directory
        assert!(!engine.check("read", "/app/.env", "/cwd").allowed);
        assert!(!engine.check("read", "/app/sub/.env", "/cwd").allowed);
        // Other files should be allowed
        assert!(engine.check("read", "/app/main.rs", "/cwd").allowed);
    }

    #[test]
    fn test_permission_glob_backward_compat_regex() {
        let engine = PermissionEngine::new(vec![
            PermissionRule {
                tool: "read".into(),
                pattern: Some(r"\.(pem|key)$".into()),
                path: None,
                action: "deny".into(),
            },
            PermissionRule {
                tool: "read".into(),
                pattern: None,
                path: None,
                action: "allow".into(),
            },
        ]);
        // Plain regex (no glob: prefix) should still work
        assert!(!engine.check("read", "/home/user/id_rsa.pem", "/cwd").allowed);
        assert!(!engine.check("read", "/home/user/id_rsa.key", "/cwd").allowed);
    }

    #[test]
    fn test_permission_multi_rule_priority() {
        // Rules from most specific to most general
        let engine = PermissionEngine::new(vec![
            PermissionRule { tool: "write".into(), pattern: None, path: Some("/etc".into()), action: "deny".into() },
            PermissionRule { tool: "write".into(), pattern: None, path: Some("/home".into()), action: "ask".into() },
            PermissionRule { tool: "write".into(), pattern: None, path: None, action: "allow".into() },
        ]);

        // Most specific: /etc -> deny
        assert!(!engine.check("write", "/etc/hosts", "/cwd").allowed);
        // Mid: /home -> ask (needs approval)
        let r = engine.check("write", "/home/user/file.txt", "/cwd");
        assert!(r.allowed && r.needs_approval);
        // General: any other write -> allow
        let r = engine.check("write", "/tmp/data.txt", "/cwd");
        assert!(r.allowed && !r.needs_approval);
    }

    #[test]
    fn test_permission_multi_rule_pattern_priority() {
        let engine = PermissionEngine::new(vec![
            PermissionRule { tool: "read".into(), pattern: Some(r"\.pem$".into()), path: None, action: "deny".into() },
            PermissionRule { tool: "read".into(), pattern: None, path: None, action: "allow".into() },
        ]);
        // .pem files -> deny
        assert!(!engine.check("read", "/home/key.pem", "/cwd").allowed);
        // Other files -> allow
        assert!(engine.check("read", "/home/main.rs", "/cwd").allowed);
    }

    #[test]
    fn test_permission_glob_deep_nested() {
        let engine = PermissionEngine::new(vec![
            PermissionRule { tool: "read".into(), pattern: Some("glob:**/*.rs".into()), path: None, action: "deny".into() },
            PermissionRule { tool: "read".into(), pattern: None, path: None, action: "allow".into() },
        ]);
        // Any .rs file should be denied (glob:**/*.rs)
        assert!(!engine.check("read", "/project/src/lib.rs", "/cwd").allowed);
        assert!(!engine.check("read", "/a/b/mod.rs", "/cwd").allowed);
        // Non-Rust files should pass
        assert!(engine.check("read", "/project/main.py", "/cwd").allowed);
    }

    #[test]
    fn test_permission_path_and_glob_combined() {
        let engine = PermissionEngine::new(vec![
            PermissionRule { tool: "write".into(), pattern: Some("glob:**/*.env".into()), path: None, action: "deny".into() },
            PermissionRule { tool: "write".into(), pattern: None, path: Some("/tmp".into()), action: "ask".into() },
            PermissionRule { tool: "write".into(), pattern: None, path: None, action: "allow".into() },
        ]);
        // Glob pattern takes priority (first rule matched)
        assert!(!engine.check("write", "/etc/.env", "/cwd").allowed);
        // Path rule: /tmp requires confirmation
        let r = engine.check("write", "/tmp/data.txt", "/cwd");
        assert!(r.allowed && r.needs_approval);
    }

    // ── Edge case tests ──

    #[test]
    fn glob_question_mark_matches_single_char() {
        let engine = PermissionEngine::new(vec![
            PermissionRule { tool: "read".into(), pattern: Some("glob:**/file.?".into()), path: None, action: "deny".into() },
        ]);
        // ? matches exactly one char
        assert!(!engine.check("read", "/dir/file.a", "/cwd").allowed);
        assert!(!engine.check("read", "/dir/file.1", "/cwd").allowed);
        // file.ab has two chars after dot
        assert!(engine.check("read", "/dir/file.ab", "/cwd").allowed);
        // No dot at all
        assert!(engine.check("read", "/dir/file", "/cwd").allowed);
    }

    #[test]
    fn glob_special_chars_are_escaped_literally() {
        // [ ] ( ) + are escaped to literal chars in regex, not regex metacharacters
        let engine = PermissionEngine::new(vec![
            PermissionRule { tool: "read".into(), pattern: Some("glob:**/file_[ab].txt".into()), path: None, action: "deny".into() },
        ]);
        // [ab] is literal — matches the actual string "file_[ab].txt"
        assert!(!engine.check("read", "/dir/file_[ab].txt", "/cwd").allowed);
        // file_a.txt does NOT match (a and b are not a regex character class here)
        assert!(engine.check("read", "/dir/file_a.txt", "/cwd").allowed);
    }

    #[test]
    fn unknown_action_defaults_to_allow() {
        let engine = PermissionEngine::new(vec![
            PermissionRule { tool: "bash".into(), pattern: None, path: None, action: "unknown-action".into() },
        ]);
        let result = engine.check("bash", "/tmp/script.sh", "/cwd");
        assert!(result.allowed);
        assert!(!result.needs_approval);
    }

    #[test]
    fn default_engine_covers_all_coding_tools() {
        let engine = PermissionEngine::defaults();
        let tools = ["read", "write", "edit", "bash", "grep"];
        for tool in &tools {
            let result = engine.check(tool, "/home/user/project/src/main.rs", "/home/user/project");
            assert!(result.allowed, "tool {tool} should be allowed on project files");
        }
        // write on project files should need approval
        let write_result = engine.check("write", "/home/user/project/src/main.rs", "/home/user/project");
        assert!(write_result.needs_approval);
    }

    #[test]
    fn empty_tool_name_is_allowed() {
        let engine = PermissionEngine::defaults();
        let result = engine.check("", "/some/path", "/cwd");
        assert!(result.allowed);
        assert!(!result.needs_approval);
    }

    #[test]
    fn path_with_special_chars() {
        let engine = PermissionEngine::defaults();
        // Paths with spaces, parens, etc.
        let result = engine.check("read", "/home/user/My Documents (v2)/file.txt", "/cwd");
        assert!(result.allowed);
        assert!(!result.needs_approval);
    }

    #[test]
    fn glob_star_does_not_cross_directory_boundary() {
        let engine = PermissionEngine::new(vec![
            PermissionRule { tool: "read".into(), pattern: Some("glob:/*/config".into()), path: None, action: "deny".into() },
        ]);
        // Single * matches within one directory
        assert!(!engine.check("read", "/app/config", "/cwd").allowed);
        assert!(!engine.check("read", "/root/config", "/cwd").allowed);
        // Should NOT match nested paths
        assert!(engine.check("read", "/app/sub/config", "/cwd").allowed);
    }

    #[test]
    fn regex_special_chars_in_path_are_escaped() {
        let engine = PermissionEngine::new(vec![
            PermissionRule { tool: "bash".into(), pattern: Some("glob:**/*.sh".into()), path: None, action: "deny".into() },
        ]);
        // Paths with regex special chars shouldn't break glob matching
        let result = engine.check("bash", "/home/user/project (v1.0+fix)/run.sh", "/cwd");
        assert!(!result.allowed, ".sh file should be denied regardless of path chars");
    }
}

