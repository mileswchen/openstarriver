#![allow(dead_code, unused)]
/// Sub-agent template configuration
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SubAgentTemplate {
    pub name: &'static str,
    pub description: &'static str,
    pub tools: &'static [&'static str],
    pub read_only: bool,
}

/// Built-in sub-agent templates
pub fn builtin_templates() -> Vec<SubAgentTemplate> {
    vec![
        SubAgentTemplate {
            name: "explorer",
            description: "Codebase exploration, read-only",
            tools: &["read", "grep"],
            read_only: true,
        },
        SubAgentTemplate {
            name: "tester",
            description: "Generate unit tests",
            tools: &["read", "write", "bash", "grep"],
            read_only: false,
        },
        SubAgentTemplate {
            name: "reviewer",
            description: "Code review, read-only",
            tools: &["read", "grep"],
            read_only: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_templates_count() {
        let templates = builtin_templates();
        assert_eq!(templates.len(), 3);
    }

    #[test]
    fn test_explorer_is_read_only() {
        let templates = builtin_templates();
        let explorer = templates.iter().find(|t| t.name == "explorer").unwrap();
        assert!(explorer.read_only);
        assert_eq!(explorer.tools, &["read", "grep"]);
    }

    #[test]
    fn test_tester_not_read_only() {
        let templates = builtin_templates();
        let tester = templates.iter().find(|t| t.name == "tester").unwrap();
        assert!(!tester.read_only);
        assert!(tester.tools.contains(&"write"));
    }

    #[test]
    fn test_reviewer_read_only() {
        let templates = builtin_templates();
        let reviewer = templates.iter().find(|t| t.name == "reviewer").unwrap();
        assert!(reviewer.read_only);
        assert_eq!(reviewer.description, "Code review, read-only");
    }

    #[test]
    fn test_all_templates_have_names() {
        let templates = builtin_templates();
        for t in &templates {
            assert!(!t.name.is_empty());
            assert!(!t.description.is_empty());
            assert!(!t.tools.is_empty());
        }
    }

    #[test]
    fn test_template_names_are_unique() {
        let templates = builtin_templates();
        let names: Vec<&str> = templates.iter().map(|t| t.name).collect();
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(names.len(), unique.len(), "template names must be unique");
    }

    #[test]
    fn test_explorer_tools_exactly_read_grep() {
        let explorer = builtin_templates()
            .into_iter()
            .find(|t| t.name == "explorer")
            .unwrap();
        assert_eq!(explorer.tools.len(), 2);
        assert_eq!(explorer.tools, &["read", "grep"]);
    }

    #[test]
    fn test_tester_tools_include_write_and_bash() {
        let tester = builtin_templates()
            .into_iter()
            .find(|t| t.name == "tester")
            .unwrap();
        assert_eq!(tester.tools.len(), 4);
        assert!(tester.tools.contains(&"read"));
        assert!(tester.tools.contains(&"write"));
        assert!(tester.tools.contains(&"bash"));
        assert!(tester.tools.contains(&"grep"));
    }

    #[test]
    fn test_reviewer_tools_exactly_read_grep() {
        let reviewer = builtin_templates()
            .into_iter()
            .find(|t| t.name == "reviewer")
            .unwrap();
        assert_eq!(reviewer.tools.len(), 2);
        assert_eq!(reviewer.tools, &["read", "grep"]);
    }

    #[test]
    fn test_read_only_templates_do_not_have_write() {
        let templates = builtin_templates();
        for t in &templates {
            if t.read_only {
                assert!(
                    !t.tools.contains(&"write"),
                    "read-only template {} should not have write",
                    t.name
                );
                assert!(
                    !t.tools.contains(&"bash"),
                    "read-only template {} should not have bash",
                    t.name
                );
            }
        }
    }

    #[test]
    fn test_builtin_templates_are_consistent_across_calls() {
        let t1 = builtin_templates();
        let t2 = builtin_templates();
        assert_eq!(t1.len(), t2.len());
        for (a, b) in t1.iter().zip(t2.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.tools, b.tools);
            assert_eq!(a.read_only, b.read_only);
        }
    }
}
