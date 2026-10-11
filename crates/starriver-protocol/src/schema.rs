//! Parameter/result schemas (a JSON Schema subset) and the validator.
//!
//! Supported subset: `type` (object/string/integer/number/boolean/array),
//! `required`, `properties`, `items`, `additionalProperties: false`.
//! Rust types are the single source of truth (schemas are declared with
//! these constructors in the method catalog); the TS client is generated
//! from the same schema — no second type definition is maintained.

use serde_json::{json, Value};

use crate::wire::ApiError;

// ── Schema constructors ──

pub fn object(props: &[(&str, Value)], required: &[&str]) -> Value {
    let mut properties = serde_json::Map::new();
    for (name, schema) in props {
        properties.insert((*name).to_string(), schema.clone());
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub fn string_schema() -> Value {
    json!({ "type": "string" })
}

pub fn integer_schema() -> Value {
    json!({ "type": "integer" })
}

pub fn number_schema() -> Value {
    json!({ "type": "number" })
}

pub fn boolean_schema() -> Value {
    json!({ "type": "boolean" })
}

pub fn array_schema(items: Value) -> Value {
    json!({ "type": "array", "items": items })
}

pub fn any_schema() -> Value {
    json!({})
}

// ── Validator ──

/// Validates `value` against `schema`; on failure returns `INVALID_PARAMS`
/// with a field path.
pub fn validate(schema: &Value, value: &Value) -> Result<(), ApiError> {
    validate_at(schema, value, "$")
}

fn validate_at(schema: &Value, value: &Value, path: &str) -> Result<(), ApiError> {
    match schema.get("type").and_then(Value::as_str) {
        None => {
            // No type = any.
            Ok(())
        }
        Some("object") => {
            let Value::Object(map) = value else {
                return Err(type_mismatch(path, "object", value));
            };
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for name in required {
                    if let Some(name) = name.as_str() {
                        if !map.contains_key(name) {
                            return Err(ApiError::invalid_params(format!(
                                "{path}.{name} is required"
                            )));
                        }
                    }
                }
            }
            if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false) {
                if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                    for key in map.keys() {
                        if !properties.contains_key(key) {
                            return Err(ApiError::invalid_params(format!(
                                "{path}.{key} is not allowed"
                            )));
                        }
                    }
                }
            }
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (key, sub) in properties {
                    if let Some(field) = map.get(key) {
                        validate_at(sub, field, &format!("{path}.{key}"))?;
                    }
                }
            }
            Ok(())
        }
        Some("string") => {
            if !value.is_string() {
                return Err(type_mismatch(path, "string", value));
            }
            Ok(())
        }
        Some("integer") => {
            if !value.is_i64() && !value.is_u64() {
                return Err(type_mismatch(path, "integer", value));
            }
            Ok(())
        }
        Some("number") => {
            if !value.is_number() {
                return Err(type_mismatch(path, "number", value));
            }
            Ok(())
        }
        Some("boolean") => {
            if !value.is_boolean() {
                return Err(type_mismatch(path, "boolean", value));
            }
            Ok(())
        }
        Some("array") => {
            let Value::Array(items) = value else {
                return Err(type_mismatch(path, "array", value));
            };
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    validate_at(item_schema, item, &format!("{path}[{index}]"))?;
                }
            }
            Ok(())
        }
        Some(other) => Err(ApiError::invalid_params(format!(
            "unsupported schema type `{other}` at {path}"
        ))),
    }
}

fn type_mismatch(path: &str, expected: &str, value: &Value) -> ApiError {
    ApiError::invalid_params(format!(
        "{path} must be {expected}, got {}",
        value_type_name(value)
    ))
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_object_shape() {
        let schema = object(
            &[
                ("task", string_schema()),
                ("sessionId", string_schema()),
                ("timeout", integer_schema()),
            ],
            &["task"],
        );
        assert!(validate(&schema, &json!({ "task": "hi" })).is_ok());
        assert!(validate(&schema, &json!({ "task": "hi", "sessionId": "s1" })).is_ok());
        assert!(validate(&schema, &json!({})).is_err());
        assert!(validate(&schema, &json!({ "task": 1 })).is_err());
        assert!(validate(&schema, &json!({ "task": "hi", "unknown": 1 })).is_err());
    }

    #[test]
    fn validates_arrays_and_nested() {
        let schema = object(&[("ids", array_schema(string_schema()))], &["ids"]);
        assert!(validate(&schema, &json!({ "ids": ["a", "b"] })).is_ok());
        assert!(validate(&schema, &json!({ "ids": ["a", 1] })).is_err());
        assert!(validate(&schema, &json!({})).is_err());
    }

    #[test]
    fn any_schema_accepts_everything() {
        assert!(validate(&any_schema(), &json!({ "x": [1, "a"] })).is_ok());
    }
}
