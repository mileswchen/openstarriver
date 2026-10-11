// Minimal markdown frontmatter (de)serialization — vendored from
// `starriver-core::profile::parser` so this crate stays core-free. Behavior is
// byte-compatible with the original (round-trip stable, marker-cleanup).

use std::collections::HashMap;

pub struct RawDocument {
    pub frontmatter: HashMap<String, serde_yaml::Value>,
    pub body: String,
}

pub fn parse_document(content: &str) -> anyhow::Result<RawDocument> {
    let content = content.trim();
    // The opening fence must be a line that is EXACTLY `---` — `----`
    // thematic breaks were swallowed as frontmatter openers and the whole
    // document rejected (same bug as skills parser BUG-19).
    if content.lines().next().map(str::trim_end) != Some("---") {
        return Ok(RawDocument {
            frontmatter: HashMap::new(),
            body: content.to_string(),
        });
    }
    let after_first = &content[3..];
    let end_idx = after_first
        .find("\n---")
        .or_else(|| after_first.find("\n---\n"));
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
    let frontmatter: HashMap<String, serde_yaml::Value> =
        serde_yaml::from_str(yaml_str).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(RawDocument {
        frontmatter,
        body: body.to_string(),
    })
}

/// Serialize frontmatter + body back to a `---`-delimited document. Strips any
/// spurious `---` / `...` document markers emitted by `serde_yaml::to_string`
/// so they do not accumulate across round trips.
pub fn serialize_document(
    fm: &HashMap<String, serde_yaml::Value>,
    body: &str,
) -> Result<String, String> {
    let yaml_str = serde_yaml::to_string(fm).map_err(|e| format!("serialize: {e}"))?;
    let clean_yaml = yaml_str
        .trim()
        .trim_start_matches("---\n")
        .trim_end_matches("\n---")
        .trim_end_matches("\n...");
    Ok(format!("---\n{}\n---\n\n{}", clean_yaml, body.trim()))
}
