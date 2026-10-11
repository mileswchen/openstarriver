use starriver_skills::profile::parser::parse_document;

#[test]
fn test_parse_full_document() {
    let content = r#"---
name: test-agent
version: 1.0.0
skills:
  - code-review
---

This is the body.
"#;
    let doc = parse_document(content).unwrap();
    assert_eq!(
        doc.frontmatter
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap(),
        "test-agent"
    );
    assert_eq!(doc.body.trim(), "This is the body.");
}

#[test]
fn test_parse_no_frontmatter() {
    let content = "Just a body text.";
    let doc = parse_document(content).unwrap();
    assert!(doc.frontmatter.is_empty());
    assert_eq!(doc.body, "Just a body text.");
}

#[test]
fn test_parse_empty_frontmatter() {
    let content = "---\n---\nBody after empty frontmatter.";
    let doc = parse_document(content).unwrap();
    assert!(doc.frontmatter.is_empty());
    assert_eq!(doc.body.trim(), "Body after empty frontmatter.");
}

#[test]
fn test_parse_invalid_yaml() {
    let content = "---\nname: [unclosed\n---\nbody";
    let result = parse_document(content);
    assert!(result.is_err());
}
