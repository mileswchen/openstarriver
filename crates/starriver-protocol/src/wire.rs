//! Wire format: request/response/event envelopes and the error model.
//!
//! Requests and responses form a one-to-one command/query channel; streaming
//! methods return events over a separate subscription event channel
//! (multiplexed by `subscription` + `seq`, supporting multiple clients and
//! replay after reconnect).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current protocol version. Clients must handshake (`protocol.info`)
/// before sending business requests.
pub const PROTOCOL_VERSION: u32 = 1;

/// Client → host request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub v: u32,
    /// Client-generated idempotency id; retries carry the same id and the
    /// command is not retriggered.
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Host → client response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub v: u32,
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

impl Response {
    pub fn ok(request: &Request, result: Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id: request.id.clone(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(request: &Request, error: ApiError) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id: request.id.clone(),
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

/// Subscription event envelope for streaming methods.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventEnvelope {
    pub v: u32,
    pub subscription: String,
    /// Session event sequence number: clients use it to order, dedupe, and
    /// detect dropped frames.
    pub seq: u64,
    /// Server-wide global event sequence number (for multi-client
    /// reconnect replay): monotonically increasing within the same
    /// daemon/embedded instance; older servers leave it unset (None), and
    /// clients skip replay accordingly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_seq: Option<u64>,
    /// Event type, reusing the host's UiEvent type vocabulary
    /// (`turnEnd`/`toolResult`/…).
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: Value,
}

impl EventEnvelope {
    pub fn new(
        subscription: impl Into<String>,
        seq: u64,
        event_type: impl Into<String>,
        payload: Value,
    ) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            subscription: subscription.into(),
            seq,
            global_seq: None,
            event_type: event_type.into(),
            payload,
        }
    }
}

/// Stable, routable error codes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(code: impl Into<String>, message: impl Into<String>, data: Value) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            data: Some(data),
        }
    }

    pub fn protocol_version(actual: u32) -> Self {
        Self::with_data(
            "PROTOCOL_VERSION",
            format!("unsupported protocol version {actual} (expected {PROTOCOL_VERSION})"),
            serde_json::json!({ "expected": PROTOCOL_VERSION, "actual": actual }),
        )
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new("METHOD_NOT_FOUND", format!("unknown method `{method}`"))
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new("INVALID_PARAMS", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new("FORBIDDEN", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("INTERNAL", message)
    }

    pub fn session_busy(session_id: &str) -> Self {
        Self::new(
            "SESSION_BUSY",
            format!("session `{session_id}` already has an active turn"),
        )
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_round_trips_over_json() {
        let request = Request {
            v: PROTOCOL_VERSION,
            id: "req_1".into(),
            method: "tools.list".into(),
            params: json!({}),
        };
        let response = Response::ok(&request, json!({ "tools": [] }));
        let text = serde_json::to_string(&response).unwrap();
        let parsed: Response = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, response);
        assert!(parsed.ok);
        assert!(parsed.error.is_none());
    }

    #[test]
    fn error_serializes_with_stable_code() {
        let error = ApiError::invalid_params("task is required");
        let text = serde_json::to_string(&error).unwrap();
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["code"], "INVALID_PARAMS");
        assert_eq!(parsed["message"], "task is required");
    }

    #[test]
    fn event_envelope_carries_subscription_and_seq() {
        let envelope = EventEnvelope::new("sub_1", 7, "turnEnd", json!({ "done": true }));
        let text = serde_json::to_string(&envelope).unwrap();
        let parsed: EventEnvelope = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.subscription, "sub_1");
        assert_eq!(parsed.seq, 7);
        assert_eq!(parsed.event_type, "turnEnd");
    }
}
