//! Host-side plan-review answerer (plan fold, plan-mode parity): the
//! `exit_plan_mode` tool presents the model's plan for review, the plan
//! surfaces as the UI's Plan confirmation card through the same
//! session-event → UiEvent chain as approvals and questions, and the
//! decision resolves the pending oneshot. UI-agnostic: any channel can call
//! [`PlanReviewBridge::respond`]; no responder → timeout → the tool fails
//! the call rather than silently leaving plan mode.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use starriver_kernel::agent::AgentRegistry;
use starriver_kernel::json::JsonValue;
use starriver_kernel::session::SessionId;
use tokio::sync::oneshot;

/// Reviews need reading time; a too-eager timeout would fail the call while
/// the user is still reading the plan. Match the approval bridge's default
/// so no interaction path waits longer than an approval would.
pub const DEFAULT_PLAN_REVIEW_TIMEOUT: Duration = Duration::from_secs(300);

/// Why a review ended without a user decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewError {
    Timeout,
    /// Session not found (e.g. already disposed when the decision came due).
    Unavailable,
}

impl ReviewError {
    /// Model-facing tag: the tool turns it into the failure the agent sees.
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewError::Timeout => "timeout",
            ReviewError::Unavailable => "unavailable",
        }
    }
}

/// The user's decision on a presented plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanDecision {
    Approved,
    /// Keep planning; the free-text feedback rides back to the model as the
    /// tool-call failure, so it revises and presents again.
    Rejected {
        feedback: Option<String>,
    },
}

struct PendingReview {
    tx: oneshot::Sender<PlanDecision>,
}

pub struct PlanReviewBridge {
    registry: Arc<dyn AgentRegistry>,
    pending: Mutex<HashMap<String, PendingReview>>,
    /// Latest request id seen per session (pending or resolved), so a UI
    /// channel can answer by session without tracking request ids.
    session_latest: Mutex<HashMap<String, String>>,
}

pub struct PlanReviewRequest {
    pub request_id: String,
}

impl PlanReviewBridge {
    pub fn new(registry: Arc<dyn AgentRegistry>) -> Self {
        Self {
            registry,
            pending: Mutex::new(HashMap::new()),
            session_latest: Mutex::new(HashMap::new()),
        }
    }

    /// Present `plan` for review and wait up to `timeout` for the user's
    /// decision. Appends the audit pair to the session log —
    /// `plan/review-requested` on ask, `plan/review-decided` on resolve or
    /// abandonment — mirroring the approval/question audit trails.
    pub async fn request(
        &self,
        session_id: &str,
        call_id: Option<&str>,
        plan: &str,
        timeout: Duration,
    ) -> Result<PlanDecision, ReviewError> {
        let Some(agent) = self.registry.get(&SessionId(session_id.to_string())) else {
            return Err(ReviewError::Unavailable);
        };
        let session = agent.session();
        let request_id = format!("pr-{}", uuid::Uuid::new_v4());
        // Insert the pending slot BEFORE the request event becomes visible:
        // a fast UI that answers off the event must not race the slot insert.
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(request_id.clone(), PendingReview { tx });
        self.session_latest
            .lock()
            .unwrap()
            .insert(session_id.to_string(), request_id.clone());
        let mut fields = vec![
            ("id".to_string(), JsonValue::String(request_id.clone())),
            ("plan".to_string(), JsonValue::String(plan.to_string())),
        ];
        if let Some(call_id) = call_id {
            fields.push(("callId".to_string(), JsonValue::String(call_id.to_string())));
        }
        if session
            .append("plan/review-requested", JsonValue::Object(fields))
            .is_err()
        {
            self.pending.lock().unwrap().remove(&request_id);
            return Err(ReviewError::Unavailable);
        }
        // Only the ask path knows the session, so it records the resolution
        // event; `respond` itself stays session-free and just resolves.
        let decided = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(decision)) => Some(decision),
            _ => None,
        };
        self.pending.lock().unwrap().remove(&request_id);
        match decided {
            Some(PlanDecision::Approved) => {
                let _ = session.append(
                    "plan/review-decided",
                    JsonValue::Object(vec![
                        ("id".into(), JsonValue::String(request_id)),
                        ("approved".into(), JsonValue::Bool(true)),
                    ]),
                );
                Ok(PlanDecision::Approved)
            }
            Some(PlanDecision::Rejected { feedback }) => {
                let mut fields = vec![
                    ("id".into(), JsonValue::String(request_id.clone())),
                    ("approved".into(), JsonValue::Bool(false)),
                ];
                if let Some(feedback) = feedback.as_deref().map(str::trim).filter(|f| !f.is_empty())
                {
                    fields.push(("feedback".into(), JsonValue::String(feedback.to_string())));
                }
                let _ = session.append("plan/review-decided", JsonValue::Object(fields));
                Ok(PlanDecision::Rejected { feedback })
            }
            None => {
                let _ = session.append(
                    "plan/review-decided",
                    JsonValue::Object(vec![
                        ("id".into(), JsonValue::String(request_id)),
                        ("approved".into(), JsonValue::Bool(false)),
                        ("abandoned".into(), JsonValue::Bool(true)),
                    ]),
                );
                Err(ReviewError::Timeout)
            }
        }
    }

    /// UI-channel decision: resolves the pending review. `false` = no such
    /// pending review; the decided event is recorded by the ask path.
    pub fn respond(&self, request_id: &str, decision: PlanDecision) -> bool {
        let tx = self
            .pending
            .lock()
            .unwrap()
            .remove(request_id)
            .map(|p| p.tx);
        match tx {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// The latest plan-review request id seen for a session (pending or
    /// already resolved).
    pub fn session_latest_request(&self, session_id: &str) -> Option<String> {
        self.session_latest.lock().unwrap().get(session_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_agent_loop::registry::InMemoryAgentRegistry;
    use starriver_kernel::agent::AgentRegistry as _;

    async fn bridge_with_session() -> (Arc<PlanReviewBridge>, Arc<InMemoryAgentRegistry>) {
        let registry = Arc::new(InMemoryAgentRegistry::new());
        let session = starriver_kernel::session::Session::new(
            SessionId("s-pr".into()),
            cordis_rslite::EventBus::new(),
        );
        registry
            .enter(crate::plan_mode::tests::FakeAgent::new(session), None)
            .expect("enter test agent");
        let bridge = Arc::new(PlanReviewBridge::new(
            Arc::clone(&registry) as Arc<dyn AgentRegistry>
        ));
        (bridge, registry)
    }

    fn request_id_of(registry: &InMemoryAgentRegistry) -> Option<String> {
        registry
            .get(&SessionId("s-pr".into()))?
            .session()
            .events()
            .into_iter()
            .rev()
            .find(|e| e.event_type == "plan/review-requested")
            .and_then(|e| e.data.as_str_at("id").map(String::from))
    }

    #[tokio::test]
    async fn request_unknown_session_is_unavailable() {
        let registry = Arc::new(InMemoryAgentRegistry::new());
        let bridge = PlanReviewBridge::new(Arc::clone(&registry) as Arc<dyn AgentRegistry>);
        let err = bridge
            .request("no-such-session", None, "# p", Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(err, ReviewError::Unavailable);
    }

    #[tokio::test]
    async fn respond_unknown_request_is_false() {
        let registry = Arc::new(InMemoryAgentRegistry::new());
        let bridge = PlanReviewBridge::new(Arc::clone(&registry) as Arc<dyn AgentRegistry>);
        assert!(!bridge.respond("missing-id", PlanDecision::Approved));
    }

    #[tokio::test]
    async fn approval_resolves_and_audits() {
        let (bridge, registry) = bridge_with_session().await;
        let ask = {
            let bridge = Arc::clone(&bridge);
            tokio::spawn(async move {
                bridge
                    .request("s-pr", Some("call-1"), "# Plan", Duration::from_secs(5))
                    .await
            })
        };
        // Wait for the pending slot, then answer off the event like a UI.
        let mut id = String::new();
        for _ in 0..100 {
            if let Some(request_id) = request_id_of(&registry) {
                id = request_id;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(bridge.respond(&id, PlanDecision::Approved));
        assert_eq!(ask.await.unwrap().unwrap(), PlanDecision::Approved);
        let events = registry
            .get(&SessionId("s-pr".into()))
            .unwrap()
            .session()
            .events();
        assert!(events
            .iter()
            .any(|e| e.event_type == "plan/review-requested"));
        let decided = events
            .iter()
            .find(|e| e.event_type == "plan/review-decided")
            .unwrap();
        assert_eq!(
            decided.data.get("approved").and_then(JsonValue::as_bool),
            Some(true)
        );
    }

    #[tokio::test]
    async fn rejection_carries_feedback_and_timeout_abandons() {
        let (bridge, registry) = bridge_with_session().await;
        let ask = {
            let bridge = Arc::clone(&bridge);
            tokio::spawn(async move {
                bridge
                    .request("s-pr", None, "# Plan", Duration::from_millis(40))
                    .await
            })
        };
        let mut id = String::new();
        for _ in 0..100 {
            if let Some(request_id) = request_id_of(&registry) {
                id = request_id;
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(bridge.respond(
            &id,
            PlanDecision::Rejected {
                feedback: Some("  add rollback notes  ".into())
            }
        ));
        assert_eq!(
            ask.await.unwrap().unwrap(),
            PlanDecision::Rejected {
                feedback: Some("  add rollback notes  ".into())
            }
        );
        let decided = registry
            .get(&SessionId("s-pr".into()))
            .unwrap()
            .session()
            .events()
            .into_iter()
            .find(|e| e.event_type == "plan/review-decided")
            .unwrap();
        assert_eq!(
            decided.data.as_str_at("feedback"),
            Some("add rollback notes"),
            "feedback is trimmed on the audit trail"
        );

        // No responder → timeout → abandoned audit, Err back to the tool.
        let err = bridge
            .request("s-pr", None, "# Plan 2", Duration::from_millis(30))
            .await
            .unwrap_err();
        assert_eq!(err, ReviewError::Timeout);
        let events = registry
            .get(&SessionId("s-pr".into()))
            .unwrap()
            .session()
            .events();
        assert!(events.iter().any(|e| {
            e.event_type == "plan/review-decided"
                && e.data.get("abandoned").and_then(JsonValue::as_bool) == Some(true)
        }));
    }

    #[test]
    fn session_latest_tracks_the_newest_request() {
        let registry = Arc::new(InMemoryAgentRegistry::new());
        let bridge = PlanReviewBridge::new(Arc::clone(&registry) as Arc<dyn AgentRegistry>);
        assert_eq!(bridge.session_latest_request("s-pr"), None);
        bridge
            .session_latest
            .lock()
            .unwrap()
            .insert("s-pr".into(), "pr-1".into());
        assert_eq!(
            bridge.session_latest_request("s-pr"),
            Some("pr-1".to_string())
        );
    }
}
