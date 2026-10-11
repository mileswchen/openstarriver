//! PTC (programmatic tool calls) prompt vocabulary: the model-facing texts
//! for the mode. Shared by the tools layer (which registers the `run_code`
//! transport and enforces the rule) and the agent loop (which renders the
//! prompt sections), so both state exactly the same contract.

use crate::llm::ToolSchema;

/// The model-facing name of the PTC mode tool (`RUN_CODE_NAME`).
pub const RUN_CODE_NAME: &str = "run_code";

/// The `tools:ptc-only` rule: the same predicate the registry's guard and the
/// schema projection enforce, so the prompt cannot state a rule the registry
/// does not apply.
pub const PTC_ONLY_TEXT: &str = "`run_code` is the only tool you can call directly — \
a tool call naming any other tool is denied. Reach every tool declared in the \
program-toolchain section from inside the program you pass to `run_code`.";

/// The `tools:ptc-sdk` section: the program dialect plus the callable tool
/// catalog. Only renders when the mode is active (the caller gates it).
pub fn sdk_section_text(schemas: &[ToolSchema]) -> String {
    let mut out = String::from(
        "PTC program toolchain: `run_code` executes a JSON program that calls tools in \
         sequence, replacing many single-tool round trips with one call.\n\
         Program document:\n\
         {\"steps\": [{\"as\": \"label\", \"call\": \"<tool>\", \"args\": {...}}, ...], \
         \"return\": \"$label\"}\n\
         - Steps run in order; a failed step is logged `ERR` and does not abort the program — \
         steps referencing its output are skipped. Any failure makes the run_code result an \
         error carrying the full log.\n\
         - Inside any args string, `$label` splices an earlier output as a typed value; \
         `$label.field.path` navigates into it; an embedded reference is JSON-encoded.\n\
         - `return` (optional) picks the program result; otherwise the step log is the output.\n\n\
         Tools callable inside the program:\n",
    );
    for schema in schemas {
        if schema.name == RUN_CODE_NAME {
            continue;
        }
        let description = schema.description.replace('\n', " ");
        out.push_str(&format!("- {} — {description}\n", schema.name));
        if let Ok(params) = schema.parameters.to_serde_json() {
            if let Some(properties) = params.get("properties").and_then(|value| value.as_object()) {
                let signature = properties
                    .iter()
                    .map(|(key, value)| format!("{key}: {}", shape_of(value, 0)))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("  args: {{{signature}}}\n"));
            }
        }
    }
    out
}

/// Compact structural type for the SDK section: `string`, `array<string>`,
/// `array<{title: string, ...}>` — one nesting level deep, then `…`. Models
/// guess argument shapes; showing the item fields removes the guessing.
fn shape_of(schema: &serde_json::Value, depth: usize) -> String {
    let kind = schema
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("json");
    // One nesting level deep, then `…` (the doc contract — BUG-66): a
    // nested ARRAY has no faithful one-level form, but an object's scalar
    // fields still render at the item level (`array<{title: string}>`).
    if depth >= 2 && kind == "array" {
        return "…".into();
    }
    if depth >= 3 && matches!(kind, "array" | "object") {
        return "…".into();
    }
    match kind {
        "array" => match schema.get("items") {
            Some(items_schema) => format!("array<{}>", shape_of(items_schema, depth + 1)),
            None => "array".to_string(),
        },
        "object" => match schema.get("properties").and_then(|value| value.as_object()) {
            Some(properties) if !properties.is_empty() => {
                let fields = properties
                    .iter()
                    .map(|(key, value)| format!("{key}: {}", shape_of(value, depth + 1)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{{fields}}}")
            }
            _ => kind.to_string(),
        },
        _ => kind.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_text_lists_tools_and_skips_run_code() {
        let schemas = vec![
            ToolSchema {
                name: "echo".into(),
                description: "Echo the input text back.".into(),
                parameters: crate::json::JsonValue::from_serde_json(serde_json::json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } }
                })),
            },
            ToolSchema {
                name: RUN_CODE_NAME.into(),
                description: "the transport".into(),
                parameters: crate::json::JsonValue::Null,
            },
        ];
        let text = sdk_section_text(&schemas);
        assert!(text.contains("- echo — Echo the input text back."));
        assert!(text.contains("args: {text: string}"));
        assert!(!text.contains("- run_code —"));
    }

    /// BUG-66 regression (round 5): the doc declares `array<string>` and
    /// `array<{title: string, ...}>` (one nesting level, then `…`) — the
    /// implementation emitted a mixed "array of<string>", never the `…`
    /// elision, and a bare "array of" tail for item-less arrays.
    #[test]
    fn shape_of_renders_the_documented_array_forms() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "tags": { "type": "array", "items": { "type": "string" } },
                "docs": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "title": { "type": "string" } }
                    }
                },
                "matrix": {
                    "type": "array",
                    "items": { "type": "array", "items": { "type": "integer" } }
                }
            }
        });
        let text = shape_of(&schema, 0);
        assert!(
            text.contains("tags: array<string>"),
            "scalar items render as array<item> (got {text})"
        );
        assert!(
            text.contains("docs: array<{title: string"),
            "object items render as array<{{field list}}> (got {text})"
        );
        assert!(
            text.contains("matrix: array<…>"),
            "deeper nesting elides to … (got {text})"
        );
        // Item-less arrays render as plain `array`, not a dangling "array of".
        assert_eq!(
            shape_of(&serde_json::json!({"type": "array"}), 0),
            "array"
        );
    }
}
