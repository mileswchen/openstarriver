//! Local copies of the two host tool helpers the bridge needs, so the crate
//! does not depend back on `starriver-host` (dependency inversion). Behavior
//! mirrors `starriver_host::tools::common`.

use cordis_rslite::Value;
use starriver_kernel::tools::ToolError;

pub(crate) fn tool_failure(tool: &str, code: &str, message: String) -> ToolError {
    ToolError::Execution {
        tool: tool.into(),
        code: code.into(),
        message,
    }
}

/// Parse the model arguments as a JSON object.
pub(crate) fn json_args(arguments: &Value) -> Result<serde_json::Value, ToolError> {
    // The agent loop carries model arguments as kernel JsonValue's runtime
    // spellings (Vec<(String, JsonValue)> for objects); tests may hand
    // serde_json::Value directly. Reconstruct a JsonValue, then convert.
    let json = match starriver_kernel::json::JsonValue::from_value(arguments) {
        Ok(json) => json,
        Err(_) => {
            let value = arguments.downcast::<serde_json::Value>().map_err(|_| {
                tool_failure(
                    "tool",
                    "INVALID_INPUT",
                    "arguments must be a JSON object".into(),
                )
            })?;
            if !value.is_object() {
                return Err(tool_failure(
                    "tool",
                    "INVALID_INPUT",
                    "arguments must be a JSON object".into(),
                ));
            }
            return Ok((*value).clone());
        }
    };
    let value = json
        .to_serde_json()
        .map_err(|e| tool_failure("tool", "INVALID_INPUT", format!("arguments not JSON: {e}")))?;
    if !value.is_object() {
        return Err(tool_failure(
            "tool",
            "INVALID_INPUT",
            "arguments must be a JSON object".into(),
        ));
    }
    Ok(value)
}

/// Render a plain-text value into a content block (definition render
/// signature: canonical value plus the finalizer's value).
pub(crate) fn text_content(
    _canonical: &Value,
    value: &Value,
) -> Vec<starriver_kernel::llm::ContentBlock> {
    use starriver_kernel::llm::ContentBlock;
    let text = if let Ok(json) = value.downcast::<starriver_kernel::json::JsonValue>() {
        json.get("output")
            .and_then(starriver_kernel::json::JsonValue::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("{json:?}"))
    } else {
        value
            .downcast::<serde_json::Value>()
            .ok()
            .and_then(|v| v.get("output").and_then(|o| o.as_str()).map(String::from))
            .unwrap_or_else(|| format!("{value:?}"))
    };
    vec![ContentBlock::Text(text)]
}
