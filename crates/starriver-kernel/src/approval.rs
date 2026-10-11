//! Approval seam interface: the tools layer depends only on this trait, so a
//! headless deployment can compose no approval service and degrade to deny.

use cordis_rslite::WaterfallEvent;
use cordis_rslite::{BoxFuture, Value};
use tokio_util::sync::CancellationToken;

/// Wire-stable approval outcomes: vocabulary plus our `allowed-similar`
/// extension (the user chose "remember this rule").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    AllowedOnce,
    AllowedSimilar,
    Rejected,
    Cancelled,
    Unavailable,
}

impl ApprovalOutcome {
    /// Stable wire tag for the durable `approval/decided` payload.
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalOutcome::AllowedOnce => "allowed-once",
            ApprovalOutcome::AllowedSimilar => "allowed-similar",
            ApprovalOutcome::Rejected => "rejected",
            ApprovalOutcome::Cancelled => "cancelled",
            ApprovalOutcome::Unavailable => "unavailable",
        }
    }

    /// Parse a wire tag; unknown tags normalize to `unavailable` (fail closed).
    pub fn parse(s: &str) -> Self {
        match s {
            "allowed-once" => ApprovalOutcome::AllowedOnce,
            "allowed-similar" => ApprovalOutcome::AllowedSimilar,
            "rejected" => ApprovalOutcome::Rejected,
            "cancelled" => ApprovalOutcome::Cancelled,
            _ => ApprovalOutcome::Unavailable,
        }
    }
}

/// One pending approval question. The tools layer carries `agent_id`
/// (== SessionId); the service resolves the agent through its registry.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub agent_id: String,
    pub tool_name: String,
    pub call_id: Option<String>,
    pub reason: Option<String>,
    pub signal: Option<CancellationToken>,
    pub arguments: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    #[error("approval request outside an open turn")]
    NoOpenTurn,
    #[error("approval service unavailable")]
    Unavailable,
    #[error("approval audit failed: {0}")]
    Audit(String),
}

/// Object-safe approval service face implemented by the agent loop. Register
/// under `Context::provide_any("approval", …)`; the tools layer reads it with
/// `Context::require_any("approval")` and `Value::downcast::<dyn ApprovalRuntime>()`.
pub trait ApprovalRuntime: Send + Sync + 'static {
    fn request(&self, req: ApprovalRequest) -> BoxFuture<Result<ApprovalOutcome, ApprovalError>>;
}

/// Sized handle over the runtime trait so a `cordis_rslite::Value` can carry
/// the service without requiring `dyn` downcast. Tools read it with
/// `Value::downcast::<ApprovalHandle>()` and deref to call `request`.
pub struct ApprovalHandle(pub std::sync::Arc<dyn ApprovalRuntime>);

impl std::ops::Deref for ApprovalHandle {
    type Target = dyn ApprovalRuntime;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

// Answerer-dispatch event: the service waterfalls this onto the agent scope;
// composed answerers (host bridge, future channels) claim one request and
// return an outcome. No answerer falls through to the fail-closed default.
cordis_rslite::cordis_event!(ApprovalRequestEvent, "approval/request", {
    req: ApprovalRequest,
    signal: Option<CancellationToken>,
});

impl WaterfallEvent for ApprovalRequestEvent {
    type Output = ApprovalOutcome;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_tags_round_trip() {
        for outcome in [
            ApprovalOutcome::AllowedOnce,
            ApprovalOutcome::AllowedSimilar,
            ApprovalOutcome::Rejected,
            ApprovalOutcome::Cancelled,
            ApprovalOutcome::Unavailable,
        ] {
            assert_eq!(ApprovalOutcome::parse(outcome.as_str()), outcome);
        }
    }

    #[test]
    fn unknown_tag_normalizes_to_unavailable() {
        assert_eq!(
            ApprovalOutcome::parse("rogue"),
            ApprovalOutcome::Unavailable
        );
    }

    #[test]
    fn approval_handle_downcasts_and_forwards() {
        struct Dummy;
        impl ApprovalRuntime for Dummy {
            fn request(
                &self,
                _: ApprovalRequest,
            ) -> BoxFuture<Result<ApprovalOutcome, ApprovalError>> {
                Box::pin(async { Ok(ApprovalOutcome::AllowedOnce) })
            }
        }
        let value = Value::from_arc(std::sync::Arc::new(ApprovalHandle(std::sync::Arc::new(
            Dummy,
        ))));
        let restored = value.downcast::<ApprovalHandle>().unwrap();
        let req = ApprovalRequest {
            agent_id: "s1".into(),
            tool_name: "bash".into(),
            call_id: None,
            reason: None,
            signal: None,
            arguments: None,
        };
        let outcome = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(restored.request(req))
            .unwrap();
        assert_eq!(outcome, ApprovalOutcome::AllowedOnce);
    }
}
