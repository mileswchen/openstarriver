//! Lossless JSON value tree used at durable/serialization boundaries.
//!
//! The runtime keeps the type-erased `cordis_rslite::Value` in memory; values
//! crossing a storage or wire boundary (session event data, tool schemas,
//! replay metadata) convert through this tree, following cordis-rs's
//! dual-value pattern (runtime `Value` + config `Node`). Object keys preserve
//! insertion order so serialized output stays stable and diff-friendly.

use cordis_rslite::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

#[derive(Debug, thiserror::Error)]
pub enum JsonValueError {
    #[error("value of type `{0}` is not lossless JSON")]
    Unsupported(&'static str),
    #[error("expected a JSON object payload")]
    NotObject,
    #[error("payload already carries the tag key `{0}`")]
    DuplicateTagKey(String),
}

impl JsonValue {
    /// Convert a runtime `Value` into the JSON tree, supporting the common
    /// primitive payloads and any already-typed `JsonValue`/`Vec<JsonValue>`.
    /// Unsupported opaque types are rejected rather than silently lossy.
    pub fn from_value(value: &Value) -> Result<JsonValue, JsonValueError> {
        macro_rules! downcast {
            ($ty:ty, $variant:ident, $cast:ty) => {
                if let Ok(inner) = value.downcast::<$ty>() {
                    return Ok(JsonValue::$variant(*inner as $cast));
                }
            };
        }
        if let Ok(inner) = value.downcast::<bool>() {
            return Ok(JsonValue::Bool(*inner));
        }
        if let Ok(inner) = value.downcast::<String>() {
            return Ok(JsonValue::String(inner.as_str().to_string()));
        }
        if let Ok(inner) = value.downcast::<JsonValue>() {
            return Ok(inner.as_ref().clone());
        }
        if let Ok(inner) = value.downcast::<Vec<JsonValue>>() {
            return Ok(JsonValue::Array(inner.as_ref().clone()));
        }
        if let Ok(inner) = value.downcast::<Vec<(String, JsonValue)>>() {
            return Ok(JsonValue::Object(inner.as_ref().clone()));
        }
        if value.downcast::<()>().is_ok() {
            return Ok(JsonValue::Null);
        }
        downcast!(i8, Int, i64);
        downcast!(i16, Int, i64);
        downcast!(i32, Int, i64);
        downcast!(i64, Int, i64);
        downcast!(isize, Int, i64);
        downcast!(u8, UInt, u64);
        downcast!(u16, UInt, u64);
        downcast!(u32, UInt, u64);
        downcast!(u64, UInt, u64);
        downcast!(usize, UInt, u64);
        downcast!(f32, Float, f64);
        downcast!(f64, Float, f64);
        Err(JsonValueError::Unsupported(value.type_name()))
    }

    /// Wrap the JSON tree back into a runtime `Value`.
    pub fn to_value(self) -> Value {
        match self {
            JsonValue::Null => Value::new(()),
            JsonValue::Bool(value) => Value::new(value),
            JsonValue::Int(value) => Value::new(value),
            JsonValue::UInt(value) => Value::new(value),
            JsonValue::Float(value) => Value::new(value),
            JsonValue::String(value) => Value::new(value),
            JsonValue::Array(value) => Value::new(value),
            JsonValue::Object(value) => Value::new(value),
        }
    }

    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Object(entries) => entries
                .iter()
                .find(|(entry_key, _)| entry_key == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsonValue::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            JsonValue::UInt(value) => Some(*value),
            JsonValue::Int(value) if *value >= 0 => Some(*value as u64),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(values) => Some(values),
            _ => None,
        }
    }

    pub fn as_str_at(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(JsonValue::as_str)
    }

    /// Prepend `{ tag_key: tag }` to an Object payload (the wire shape
    /// `{ type/kind, ...payload }`). Rejects non-Object payloads.
    pub fn with_tag(
        tag_key: &str,
        tag: &str,
        payload: JsonValue,
    ) -> Result<JsonValue, JsonValueError> {
        match payload {
            JsonValue::Object(mut entries) => {
                // A payload that already carries the tag key would produce
                // a duplicate key — split_tag then strips BOTH on read and
                // the payload silently loses a field. Reject at emission.
                if let Some((key, _)) = entries.iter().find(|(key, _)| key == tag_key) {
                    return Err(JsonValueError::DuplicateTagKey(key.clone()));
                }
                entries.insert(0, (tag_key.to_string(), JsonValue::String(tag.into())));
                Ok(JsonValue::Object(entries))
            }
            _ => Err(JsonValueError::NotObject),
        }
    }

    /// Split a tagged object into `(tag, remaining fields)`.
    pub fn split_tag(&self, tag_key: &str) -> Option<(String, JsonValue)> {
        match self {
            JsonValue::Object(entries) => {
                let (_, tag) = entries.iter().find(|(key, _)| key == tag_key)?;
                let tag = tag.as_str()?.to_string();
                let remaining = JsonValue::Object(
                    entries
                        .iter()
                        .filter(|(key, _)| key != tag_key)
                        .cloned()
                        .collect(),
                );
                Some((tag, remaining))
            }
            _ => None,
        }
    }

    /// Serialize this lossless tree to a compact JSON text. Non-finite
    /// floats are rejected rather than serialized lossily.
    pub fn to_json_string(&self) -> Result<String, JsonValueError> {
        Ok(self.to_serde()?.to_string())
    }

    /// Parse compact JSON text back into the lossless tree.
    pub fn from_json_str(text: &str) -> Result<Self, JsonValueError> {
        let value = serde_json::from_str::<serde_json::Value>(text)
            .map_err(|_| JsonValueError::Unsupported("malformed json text"))?;
        Ok(Self::from_serde_json(value))
    }

    /// Convert a `serde_json::Value` into the lossless tree.
    pub fn from_serde_json(value: serde_json::Value) -> Self {
        // `from_serde` is infallible: every serde_json value (including all
        // representable numbers) maps onto the lossless tree.
        Self::from_serde(value).expect("serde_json values map onto JsonValue")
    }

    /// Lossless conversion to `serde_json::Value` (the wire shape tools parse).
    pub fn to_serde_json(&self) -> Result<serde_json::Value, JsonValueError> {
        self.to_serde()
    }

    fn to_serde(&self) -> Result<serde_json::Value, JsonValueError> {
        Ok(match self {
            JsonValue::Null => serde_json::Value::Null,
            JsonValue::Bool(value) => serde_json::Value::Bool(*value),
            JsonValue::Int(value) => serde_json::Value::Number((*value).into()),
            JsonValue::UInt(value) => serde_json::Value::Number((*value).into()),
            JsonValue::Float(value) => {
                let number = serde_json::Number::from_f64(*value)
                    .ok_or(JsonValueError::Unsupported("non-finite float"))?;
                serde_json::Value::Number(number)
            }
            JsonValue::String(value) => serde_json::Value::String(value.clone()),
            JsonValue::Array(values) => serde_json::Value::Array(
                values
                    .iter()
                    .map(JsonValue::to_serde)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            JsonValue::Object(entries) => serde_json::Value::Object(
                entries
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), value.to_serde()?)))
                    .collect::<Result<serde_json::Map<_, _>, JsonValueError>>()?,
            ),
        })
    }

    fn from_serde(value: serde_json::Value) -> Result<Self, JsonValueError> {
        Ok(match value {
            serde_json::Value::Null => JsonValue::Null,
            serde_json::Value::Bool(value) => JsonValue::Bool(value),
            serde_json::Value::Number(number) => {
                if let Some(value) = number.as_i64() {
                    JsonValue::Int(value)
                } else if let Some(value) = number.as_u64() {
                    JsonValue::UInt(value)
                } else {
                    JsonValue::Float(
                        number
                            .as_f64()
                            .ok_or(JsonValueError::Unsupported("unrepresentable number"))?,
                    )
                }
            }
            serde_json::Value::String(value) => JsonValue::String(value),
            serde_json::Value::Array(values) => JsonValue::Array(
                values
                    .into_iter()
                    .map(JsonValue::from_serde)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            serde_json::Value::Object(map) => JsonValue::Object(
                map.into_iter()
                    .map(|(key, value)| Ok((key, JsonValue::from_serde(value)?)))
                    .collect::<Result<Vec<_>, JsonValueError>>()?,
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_value_covers_common_primitives() {
        assert_eq!(
            JsonValue::from_value(&Value::new("hi".to_string())).unwrap(),
            JsonValue::String("hi".into())
        );
        assert_eq!(
            JsonValue::from_value(&Value::new(true)).unwrap(),
            JsonValue::Bool(true)
        );
        assert_eq!(
            JsonValue::from_value(&Value::new(42i64)).unwrap(),
            JsonValue::Int(42)
        );
        assert_eq!(
            JsonValue::from_value(&Value::new(7u32)).unwrap(),
            JsonValue::UInt(7)
        );
        assert_eq!(
            JsonValue::from_value(&Value::new(1.5f64)).unwrap(),
            JsonValue::Float(1.5)
        );
        assert_eq!(
            JsonValue::from_value(&Value::new(())).unwrap(),
            JsonValue::Null
        );
    }

    #[test]
    fn from_value_rejects_opaque_types() {
        struct Opaque;
        let error = JsonValue::from_value(&Value::new(Opaque)).unwrap_err();
        assert!(matches!(error, JsonValueError::Unsupported(_)));
    }

    #[test]
    fn from_serde_json_roundtrips_serde_values() {
        let value = serde_json::json!({
            "type": "object",
            "properties": {"url": {"type": "string"}},
            "required": ["url"],
        });
        let tree = JsonValue::from_serde_json(value.clone());
        assert_eq!(tree.to_serde_json().unwrap(), value);
    }

    #[test]
    fn to_value_round_trips() {
        let json = JsonValue::Object(vec![
            ("name".into(), JsonValue::String("echo".into())),
            ("count".into(), JsonValue::Int(3)),
        ]);
        let back = JsonValue::from_value(&json.clone().to_value()).unwrap();
        assert_eq!(back, json);
        assert_eq!(back.get("name"), Some(&JsonValue::String("echo".into())));
        assert_eq!(back.get("missing"), None);
    }

    #[test]
    fn tag_helpers_round_trip_object_payload() {
        let payload = JsonValue::Object(vec![
            ("url".into(), JsonValue::String("https://x".into())),
            ("width".into(), JsonValue::Int(640)),
        ]);
        let tagged = JsonValue::with_tag("type", "video", payload).unwrap();
        assert_eq!(tagged.as_str_at("type"), Some("video"));
        let (tag, fields) = tagged.split_tag("type").unwrap();
        assert_eq!(tag, "video");
        assert_eq!(
            fields.get("url").and_then(JsonValue::as_str),
            Some("https://x")
        );
    }

    #[test]
    fn with_tag_rejects_non_object_payload() {
        let error =
            JsonValue::with_tag("type", "video", JsonValue::String("raw".into())).unwrap_err();
        assert!(matches!(error, JsonValueError::NotObject));
    }

    #[test]
    fn accessors_read_primitives() {
        assert_eq!(JsonValue::String("s".into()).as_str(), Some("s"));
        assert_eq!(JsonValue::Bool(true).as_bool(), Some(true));
        assert_eq!(
            JsonValue::Array(vec![JsonValue::Null])
                .as_array()
                .map(|array| array.len()),
            Some(1)
        );
        assert_eq!(JsonValue::Int(1).as_str(), None);
        assert_eq!(JsonValue::UInt(7).as_u64(), Some(7));
    }

    #[test]
    fn json_string_round_trips_losslessly() {
        let json = JsonValue::Object(vec![
            ("a".into(), JsonValue::Int(1)),
            ("b".into(), JsonValue::Float(1.5)),
            // A UInt beyond i64::MAX survives the text boundary (serde_json
            // cannot represent it as i64), proving lossless round-trip.
            ("u".into(), JsonValue::UInt(u64::MAX)),
            (
                "c".into(),
                JsonValue::Array(vec![JsonValue::Null, JsonValue::Bool(true)]),
            ),
        ]);
        let text = json.to_json_string().unwrap();
        let back = JsonValue::from_json_str(&text).unwrap();
        assert_eq!(back, json);
    }

    #[test]
    fn json_string_rejects_non_finite_float() {
        assert!(JsonValue::Float(f64::NAN).to_json_string().is_err());
        assert!(JsonValue::Float(f64::INFINITY).to_json_string().is_err());
    }

    #[test]
    fn json_string_parses_malformed_input_as_error() {
        assert!(JsonValue::from_json_str("{not json").is_err());
    }

    #[test]
    fn json_string_round_trips_negative_and_large_integers() {
        let json = JsonValue::Object(vec![
            ("neg".into(), JsonValue::Int(-42)),
            ("i64_min".into(), JsonValue::Int(i64::MIN)),
            ("i64_max".into(), JsonValue::Int(i64::MAX)),
            ("u64_max".into(), JsonValue::UInt(u64::MAX)),
            ("zero".into(), JsonValue::Int(0)),
        ]);
        let text = json.to_json_string().unwrap();
        let back = JsonValue::from_json_str(&text).unwrap();
        assert_eq!(back, json);
    }

    #[test]
    fn json_string_round_trips_unicode_and_empty_values() {
        let json = JsonValue::Object(vec![
            ("s".into(), JsonValue::String("中文 🚀 \u{1F600}".into())),
            ("empty".into(), JsonValue::String(String::new())),
            (
                "nested".into(),
                JsonValue::Array(vec![JsonValue::Array(vec![]), JsonValue::Object(vec![])]),
            ),
        ]);
        let text = json.to_json_string().unwrap();
        let back = JsonValue::from_json_str(&text).unwrap();
        assert_eq!(back, json);
    }

    #[test]
    fn json_string_round_trips_float_precision() {
        let json = JsonValue::Object(vec![
            ("f".into(), JsonValue::Float(std::f64::consts::PI)),
            ("neg".into(), JsonValue::Float(-0.0)),
            ("exp".into(), JsonValue::Float(1e300)),
        ]);
        let text = json.to_json_string().unwrap();
        let back = JsonValue::from_json_str(&text).unwrap();
        assert_eq!(back, json);
    }
}
