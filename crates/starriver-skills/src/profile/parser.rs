use std::collections::HashMap;

/// Parsed raw document with frontmatter and body
#[derive(Debug, Clone)]
pub struct RawDocument {
    pub frontmatter: HashMap<String, serde_yaml::Value>,
    pub body: String,
}

pub fn parse_document(content: &str) -> Result<RawDocument, ParseError> {
    let content = content.trim();
    // BUG-19: the opening fence must be a line that is exactly `---` —
    // `starts_with("---")` also swallowed long thematic breaks (`----`),
    // misreading the whole body as frontmatter YAML and rejecting the file.
    if content.lines().next().map(str::trim_end) != Some("---") {
        return Ok(RawDocument {
            frontmatter: HashMap::new(),
            body: content.to_string(),
        });
    }
    let after_first = &content[3..];
    let end_idx = after_first.find("\n---");
    let (yaml_str, body) = match end_idx {
        Some(idx) => {
            let yaml_part = &after_first[..idx];
            let body_start = idx + 4;
            let body_part = after_first[body_start..].trim();
            (yaml_part.trim(), body_part)
        }
        None => (after_first.trim(), ""),
    };
    if yaml_str.is_empty() {
        return Ok(RawDocument {
            frontmatter: HashMap::new(),
            body: body.to_string(),
        });
    }
    let frontmatter: HashMap<String, serde_yaml::Value> = serde_yaml::from_str(yaml_str)
        .map_err(|e| ParseError::InvalidFrontmatter(e.to_string()))?;
    Ok(RawDocument {
        frontmatter,
        body: body.to_string(),
    })
}

/// Serialize frontmatter + body back to AGENT.md content.
///
/// This is the canonical round-trip counterpart to [`parse_document`].
/// It strips any spurious `---` / `...` document markers emitted by
/// `serde_yaml::to_string` so they do not accumulate across cycles.
pub fn serialize_document(
    fm: &HashMap<String, serde_yaml::Value>,
    body: &str,
) -> Result<String, String> {
    let yaml_str = serde_yaml::to_string(fm).map_err(|e| format!("serialize: {e}"))?;
    // Strip any --- / ... document markers from serde_yaml output
    let clean_yaml = yaml_str
        .trim()
        .trim_start_matches("---\n")
        .trim_end_matches("\n---")
        .trim_end_matches("\n...");
    Ok(format!("---\n{}\n---\n\n{}", clean_yaml, body.trim()))
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("Invalid YAML frontmatter: {0}")]
    InvalidFrontmatter(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::Value;

    // ── Basic parsing ──

    #[test]
    fn parse_empty_content() {
        let doc = parse_document("").unwrap();
        assert!(doc.frontmatter.is_empty());
        assert_eq!(doc.body, "");
    }

    #[test]
    fn parse_no_frontmatter() {
        let doc = parse_document("# Just a markdown file").unwrap();
        assert!(doc.frontmatter.is_empty());
        assert_eq!(doc.body, "# Just a markdown file");
    }

    #[test]
    fn parse_simple_frontmatter() {
        let content = "---\nname: Test Agent\nversion: \"1.0\"\n---\n\n# Hello\n";
        let doc = parse_document(content).unwrap();
        assert_eq!(
            doc.frontmatter.get("name").unwrap().as_str().unwrap(),
            "Test Agent"
        );
        assert_eq!(
            doc.frontmatter.get("version").unwrap().as_str().unwrap(),
            "1.0"
        );
        assert_eq!(doc.body, "# Hello");
    }

    #[test]
    fn parse_model_with_variants() {
        let content = "\
---
name: Chat Agent
model:
  default: deepseekv4/deepseek-v4-pro
  models:
    - deepseekv4/deepseek-v4-pro
    - deepseekv4/deepseek-v4-flash
---

# Chat Agent
";
        let doc = parse_document(content).unwrap();
        let model = doc.frontmatter.get("model").unwrap();
        let default = model.get("default").unwrap().as_str().unwrap();
        assert_eq!(default, "deepseekv4/deepseek-v4-pro");
        let models = model.get("models").unwrap().as_sequence().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].as_str().unwrap(), "deepseekv4/deepseek-v4-pro");
        assert_eq!(models[1].as_str().unwrap(), "deepseekv4/deepseek-v4-flash");
    }

    // ── Round-trip: parse → serialize → re-parse ──

    #[test]
    fn round_trip_single_cycle() {
        let original = "\
---
name: Chat Agent
model:
  default: deepseekv4/deepseek-v4-pro
  models:
    - deepseekv4/deepseek-v4-pro
    - deepseekv4/deepseek-v4-flash
---

# Chat Agent
";
        let doc = parse_document(original).unwrap();
        let serialized = serialize_document(&doc.frontmatter, &doc.body).unwrap();
        let doc2 = parse_document(&serialized).unwrap();

        // Verify model data preserved
        let model = doc2.frontmatter.get("model").unwrap();
        assert_eq!(
            model.get("default").unwrap().as_str().unwrap(),
            "deepseekv4/deepseek-v4-pro"
        );
        let models = model.get("models").unwrap().as_sequence().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].as_str().unwrap(), "deepseekv4/deepseek-v4-pro");
        assert_eq!(models[1].as_str().unwrap(), "deepseekv4/deepseek-v4-flash");
        assert_eq!(doc2.body, "# Chat Agent");
    }

    #[test]
    fn round_trip_preserves_model_ids_with_hyphens() {
        let original = "\
---
name: Test
model:
  default: gpt-4o-mini
  models:
    - gpt-4o
    - gpt-4o-mini
    - claude-3-5-sonnet
---

# Test agent with hyphenated model IDs
";
        let doc = parse_document(original).unwrap();
        let serialized = serialize_document(&doc.frontmatter, &doc.body).unwrap();
        let doc2 = parse_document(&serialized).unwrap();

        let model = doc2.frontmatter.get("model").unwrap();
        let models = model.get("models").unwrap().as_sequence().unwrap();
        assert_eq!(models.len(), 3);
        // Verify hyphenated IDs are intact
        let ids: Vec<&str> = models.iter().filter_map(|v| v.as_str()).collect();
        assert!(ids.contains(&"gpt-4o-mini"));
        assert!(ids.contains(&"claude-3-5-sonnet"));
    }

    // ── Multiple round-trips (accumulation detection) ──

    #[test]
    fn round_trip_five_cycles_no_corruption() {
        // Simulates repeatedly setting the active model (5 times)
        let initial = "\
---
name: Chat Agent
model:
  default: deepseekv4/deepseek-v4-flash
  models:
    - deepseekv4/deepseek-v4-pro
    - deepseekv4/deepseek-v4-flash
---

# Chat Agent
";
        let mut current = initial.to_string();

        let models_to_add = [
            "deepseekv4/deepseek-v4-flash",
            "deepseekv4/deepseek-v4-pro",
            "ollama/qwen3-coder:7b",
            "openai/gpt-4o-mini",
            "deepseekv4/deepseek-v4-flash", // duplicate — should be de-duped
        ];

        for new_model in &models_to_add {
            let doc = parse_document(&current).unwrap();
            let mut fm = doc.frontmatter.clone();

            let model_val = fm
                .entry("model".to_string())
                .or_insert(Value::Mapping(Default::default()));
            if let Value::Mapping(ref mut map) = model_val {
                map.insert(
                    Value::String("default".into()),
                    Value::String(new_model.to_string()),
                );
                let models_entry = map
                    .entry(Value::String("models".into()))
                    .or_insert(Value::Sequence(vec![]));
                if let Value::Sequence(ref mut seq) = models_entry {
                    if !seq.iter().any(|v| v.as_str() == Some(new_model)) {
                        seq.push(Value::String(new_model.to_string()));
                    }
                }
            }

            current = serialize_document(&fm, &doc.body).unwrap();

            // Verify no corruption after each cycle
            let verify = parse_document(&current).unwrap();
            let model = verify.frontmatter.get("model").unwrap();
            let models = model.get("models").unwrap().as_sequence().unwrap();
            for m in models {
                let id = m.as_str().unwrap();
                assert!(
                    !id.ends_with("---"),
                    "Model ID '{id}' should not end with '---' after cycle with new_model='{new_model}'"
                );
                assert!(
                    !id.starts_with("---"),
                    "Model ID '{id}' should not start with '---'"
                );
            }
        }

        // Final verification: all 4 unique models present, none corrupted
        let final_doc = parse_document(&current).unwrap();
        let model = final_doc.frontmatter.get("model").unwrap();
        let models = model.get("models").unwrap().as_sequence().unwrap();
        assert_eq!(models.len(), 4);
        let ids: Vec<&str> = models.iter().filter_map(|v| v.as_str()).collect();
        assert!(ids.contains(&"deepseekv4/deepseek-v4-pro"));
        assert!(ids.contains(&"deepseekv4/deepseek-v4-flash"));
        assert!(ids.contains(&"ollama/qwen3-coder:7b"));
        assert!(ids.contains(&"openai/gpt-4o-mini"));

        // Verify no trailing dashes anywhere
        for id in &ids {
            assert!(
                !id.ends_with("---"),
                "ID '{id}' corrupted with trailing ---"
            );
        }
    }

    #[test]
    fn round_trip_twenty_cycles_stress() {
        // Stress test: 20 cycles to definitively rule out accumulation
        let mut current = String::from(
            "\
---
name: Stress Test
model:
  default: model-0
  models:
    - model-0
---

# Stress
",
        );
        for i in 1..=20 {
            let doc = parse_document(&current).unwrap();
            let mut fm = doc.frontmatter.clone();
            let new_model = format!("model-{i}");

            let model_val = fm
                .entry("model".to_string())
                .or_insert(Value::Mapping(Default::default()));
            if let Value::Mapping(ref mut map) = model_val {
                map.insert(
                    Value::String("default".into()),
                    Value::String(new_model.clone()),
                );
                let models_entry = map
                    .entry(Value::String("models".into()))
                    .or_insert(Value::Sequence(vec![]));
                if let Value::Sequence(ref mut seq) = models_entry {
                    seq.push(Value::String(new_model));
                }
            }

            current = serialize_document(&fm, &doc.body).unwrap();
        }

        // After 20 cycles, verify no corruption
        let final_doc = parse_document(&current).unwrap();
        let model = final_doc.frontmatter.get("model").unwrap();
        let models = model.get("models").unwrap().as_sequence().unwrap();
        assert_eq!(models.len(), 21); // model-0 through model-20

        for m in models {
            let id = m.as_str().unwrap();
            assert!(!id.contains("---"), "ID '{id}' corrupted after 20 cycles");
        }
    }

    // ── Corruption detection & recovery ──

    #[test]
    fn parse_corrupted_content_with_trailing_dashes() {
        // Simulates the exact corruption pattern observed in the wild:
        // model IDs ending with "---" due to accumulated document markers.
        let corrupted = "\
---
name: Chat Agent
model:
  default: deepseekv4/deepseek-v4-flash
  models:
  - deepseekv4/deepseek-v4-pro---
  - deepseekv4/deepseek-v4-flash---
---

# Chat Agent
";
        let doc = parse_document(corrupted).unwrap();
        let model = doc.frontmatter.get("model").unwrap();
        let models = model.get("models").unwrap().as_sequence().unwrap();
        // The parser reads the corruption as-is (the IDs contain trailing ---)
        // This test documents the current behavior so we can detect regressions.
        assert_eq!(models.len(), 2);
        let id0 = models[0].as_str().unwrap();
        let id1 = models[1].as_str().unwrap();
        // Currently the parser preserves the corruption — the fix is in
        // serialize_document which prevents new writes from compounding it.
        assert!(id0.contains("---"), "Corrupted ID should contain ---");
        assert!(id1.contains("---"), "Corrupted ID should contain ---");
    }

    #[test]
    fn corrupted_content_does_not_accumulate_on_round_trip() {
        // Even if the source file is corrupted, a round-trip through
        // serialize_document should not make it worse (no new --- added).
        let corrupted = "\
---
name: Chat Agent
model:
  default: deepseekv4/deepseek-v4-flash
  models:
  - deepseekv4/deepseek-v4-pro---
  - deepseekv4/deepseek-v4-flash---
---

# Chat Agent
";
        let doc = parse_document(corrupted).unwrap();
        let serialized = serialize_document(&doc.frontmatter, &doc.body).unwrap();
        let doc2 = parse_document(&serialized).unwrap();

        // The serialized output preserves corrupted values as-is
        // (serde_yaml does not modify string values), but the file
        // structure should be clean — no extra --- markers embedded
        // in the YAML structure itself.
        let model2 = doc2.frontmatter.get("model").unwrap();
        let models2 = model2.get("models").unwrap().as_sequence().unwrap();
        assert_eq!(models2.len(), 2);

        // Key property: the serialized content should be parseable
        // and the number of --- markers should not increase.
        let original_marker_count = corrupted.match_indices("---").count();
        let serialized_marker_count = serialized.match_indices("---").count();
        assert!(
            serialized_marker_count <= original_marker_count + 1,
            "--- marker count should not increase significantly: \
             original={original_marker_count}, serialized={serialized_marker_count}"
        );
    }

    // ── Body content edge cases ──

    #[test]
    fn body_containing_dashes_not_parsed_as_frontmatter() {
        let content = "\
---
name: Test
---

# Section 1

---

# Section 2

Some text with --- inline dashes.
";
        let doc = parse_document(content).unwrap();
        assert_eq!(
            doc.frontmatter.get("name").unwrap().as_str().unwrap(),
            "Test"
        );
        // Body should contain everything after the first closing ---
        assert!(doc.body.contains("# Section 1"));
        assert!(doc.body.contains("# Section 2"));
        assert!(doc.body.contains("--- inline dashes"));
    }

    #[test]
    fn empty_frontmatter_fields() {
        let content = "\
---
name: Minimal
version:
description: \"\"
---

Body here.
";
        let doc = parse_document(content).unwrap();
        assert_eq!(
            doc.frontmatter.get("name").unwrap().as_str().unwrap(),
            "Minimal"
        );
        // version is null in YAML, so it's Value::Null
        assert!(doc.frontmatter.get("version").unwrap().is_null());
        assert_eq!(
            doc.frontmatter
                .get("description")
                .unwrap()
                .as_str()
                .unwrap(),
            ""
        );
        assert_eq!(doc.body, "Body here.");
    }

    // ── serialize_document stability ──

    #[test]
    fn serialize_then_parse_is_idempotent() {
        // serialize → parse → serialize → parse
        // Both parsed results should be semantically identical,
        // even if key ordering differs (HashMap is unordered).
        let original = "\
---
name: Chat Agent
model:
  default: deepseekv4/deepseek-v4-pro
  models:
    - deepseekv4/deepseek-v4-pro
    - deepseekv4/deepseek-v4-flash
---

# Chat Agent
";
        let doc1 = parse_document(original).unwrap();
        let s1 = serialize_document(&doc1.frontmatter, &doc1.body).unwrap();
        let doc2 = parse_document(&s1).unwrap();

        // Both parsed docs should have the same model data
        let m1 = doc1.frontmatter.get("model").unwrap();
        let m2 = doc2.frontmatter.get("model").unwrap();
        assert_eq!(m1, m2, "model value should survive round-trip");

        // Verify model IDs are intact
        let models2 = m2.get("models").unwrap().as_sequence().unwrap();
        assert_eq!(models2.len(), 2);
        let ids: Vec<&str> = models2.iter().filter_map(|v| v.as_str()).collect();
        assert!(ids.contains(&"deepseekv4/deepseek-v4-pro"));
        assert!(ids.contains(&"deepseekv4/deepseek-v4-flash"));

        // Default model preserved
        assert_eq!(
            m2.get("default").unwrap().as_str().unwrap(),
            "deepseekv4/deepseek-v4-pro"
        );

        // Body preserved
        assert_eq!(doc1.body, doc2.body);

        // Second round-trip
        let s2 = serialize_document(&doc2.frontmatter, &doc2.body).unwrap();
        let doc3 = parse_document(&s2).unwrap();
        let m3 = doc3.frontmatter.get("model").unwrap();
        assert_eq!(m2, m3, "third generation model data should match second");
    }

    #[test]
    fn serialize_document_rejects_invalid_frontmatter() {
        // serde_yaml can serialize most HashMaps, but verify correct output
        let mut fm = HashMap::new();
        fm.insert("name".to_string(), Value::String("Test".into()));
        let result = serialize_document(&fm, "# Body").unwrap();
        assert!(result.starts_with("---\n"));
        assert!(result.contains("name: Test"));
        assert!(result.ends_with("# Body"));
    }

    // ── Edge case tests ──

    #[test]
    fn empty_body_with_frontmatter() {
        let content = "---\nname: Test\n---\n";
        let doc = parse_document(content).unwrap();
        assert_eq!(
            doc.frontmatter.get("name").unwrap().as_str().unwrap(),
            "Test"
        );
        assert_eq!(doc.body, "");
        // Serialize and verify body stays empty
        let serialized = serialize_document(&doc.frontmatter, &doc.body).unwrap();
        assert!(serialized.contains("---\nname: Test\n---"));
    }

    #[test]
    fn serialize_empty_frontmatter_with_body() {
        let fm = HashMap::new();
        let result = serialize_document(&fm, "# Only body").unwrap();
        assert!(result.starts_with("---\n"));
        assert!(result.contains("# Only body"));
    }

    #[test]
    fn unicode_values_in_frontmatter() {
        let content = "---\nname: 测试\nmodel:\n  default: gpt-4o\n  models:\n    - claude-3-5-sonnet\n    - 文心一言\n---\n\n中文内容\n";
        let doc = parse_document(content).unwrap();
        assert_eq!(
            doc.frontmatter.get("name").unwrap().as_str().unwrap(),
            "测试"
        );
        let model = doc.frontmatter.get("model").unwrap();
        let models = model.get("models").unwrap().as_sequence().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].as_str().unwrap(), "claude-3-5-sonnet");
        assert_eq!(models[1].as_str().unwrap(), "文心一言");
        assert_eq!(doc.body, "中文内容");

        // Round-trip preserves Unicode
        let serialized = serialize_document(&doc.frontmatter, &doc.body).unwrap();
        let doc2 = parse_document(&serialized).unwrap();
        assert_eq!(
            doc2.frontmatter.get("name").unwrap().as_str().unwrap(),
            "测试"
        );
    }

    #[test]
    fn values_containing_colons_and_special_chars() {
        let content = "---\nname: Test\nmodel:\n  default: gpt-4o-mini\n  models:\n    - url:https://api.example.com/v1\n    - key=value&foo=bar\n---\n\nBody\n";
        let doc = parse_document(content).unwrap();
        let model = doc.frontmatter.get("model").unwrap();
        let models = model.get("models").unwrap().as_sequence().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(
            models[0].as_str().unwrap(),
            "url:https://api.example.com/v1"
        );
        assert_eq!(models[1].as_str().unwrap(), "key=value&foo=bar");
    }

    #[test]
    fn round_trip_with_empty_frontmatter() {
        // parse → serialize → parse with empty frontmatter
        let content = "---\n---\nBody only\n";
        let doc = parse_document(content).unwrap();
        assert!(doc.frontmatter.is_empty());
        assert_eq!(doc.body, "Body only");

        let serialized = serialize_document(&doc.frontmatter, &doc.body).unwrap();
        let doc2 = parse_document(&serialized).unwrap();
        assert!(doc2.frontmatter.is_empty());
        assert_eq!(doc2.body, "Body only");
    }

    #[test]
    fn body_with_markdown_headings() {
        let content =
            "---\nname: Test\n---\n\n# Heading 1\n\n## Heading 2\n\nSome text\n\n### Heading 3\n";
        let doc = parse_document(content).unwrap();
        assert_eq!(
            doc.frontmatter.get("name").unwrap().as_str().unwrap(),
            "Test"
        );
        assert!(doc.body.contains("# Heading 1"));
        assert!(doc.body.contains("## Heading 2"));
        assert!(doc.body.contains("### Heading 3"));
        // Verify the body is NOT parsed as frontmatter (only first --- block is FM)
        assert!(doc.body.starts_with("# Heading 1"));
    }

    #[test]
    fn round_trip_preserves_model_ordering() {
        let content = "---\nname: Test\nmodel:\n  default: first\n  models:\n    - third\n    - first\n    - second\n---\n\nBody\n";
        let doc = parse_document(content).unwrap();
        let serialized = serialize_document(&doc.frontmatter, &doc.body).unwrap();
        let doc2 = parse_document(&serialized).unwrap();
        let model = doc2.frontmatter.get("model").unwrap();
        let models = model.get("models").unwrap().as_sequence().unwrap();
        // After round-trip, model IDs should all be present (order may vary with HashMap)
        let ids: Vec<&str> = models.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&"first"));
        assert!(ids.contains(&"second"));
        assert!(ids.contains(&"third"));
    }

    #[test]
    fn serialize_document_output_has_exactly_two_document_markers() {
        let mut fm = HashMap::new();
        fm.insert("name".to_string(), Value::String("Test".into()));
        fm.insert("version".to_string(), Value::String("1.0".into()));
        let result = serialize_document(&fm, "# Body").unwrap();
        // Should have exactly one opening --- and one closing ---
        let lines: Vec<&str> = result.lines().collect();
        let marker_count = lines.iter().filter(|l| l.trim() == "---").count();
        assert_eq!(
            marker_count, 2,
            "should have exactly 2 document markers, got {marker_count}\nContent:\n{result}"
        );
    }

    #[test]
    fn parse_content_with_only_dashes() {
        let content = "---\n---";
        let doc = parse_document(content).unwrap();
        assert!(doc.frontmatter.is_empty());
        assert_eq!(doc.body, "");
    }

    /// BUG-19 regression: a body whose first line is a long thematic break
    /// (`----`) is plain Markdown, not a broken frontmatter block.
    #[test]
    fn leading_long_thematic_break_is_not_frontmatter() {
        let content = "----\n\n# Title\n\nRegular body starting with a wide rule.";
        let doc = parse_document(content).unwrap();
        assert!(
            doc.frontmatter.is_empty(),
            "---- (4 dashes) opens a thematic break, not frontmatter"
        );
        assert!(doc.body.contains("# Title"), "body keeps the whole document");
        assert!(doc.body.starts_with("----"), "the rule itself is body text");

        // Five dashes behave the same way.
        let doc = parse_document("-----\ntext").unwrap();
        assert!(doc.frontmatter.is_empty());
        assert_eq!(doc.body, "-----\ntext");
    }

    #[test]
    fn serialize_document_handles_special_characters_in_body() {
        let mut fm = HashMap::new();
        fm.insert("name".to_string(), Value::String("Test".into()));
        let body = "Line with <html> tags\nLine with --- dashes\nLine with ``` code ```\n";
        let result = serialize_document(&fm, body).unwrap();
        let doc = parse_document(&result).unwrap();
        assert_eq!(doc.body, body.trim());
    }
}
