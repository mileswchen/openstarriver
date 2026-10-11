//! Host-side approval answerer: listens on `approval/request`, resolves the
//! pending oneshot when the UI responds. UI-agnostic: any channel can call
//! `respond` / the legacy `approve` entry.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cordis_rslite::EventBus;
use starriver_kernel::approval::{ApprovalOutcome, ApprovalRequestEvent};
use starriver_kernel::json::JsonValue;
use starriver_kernel::session::SessionId;
use tokio::sync::oneshot;

use starriver_kernel::agent::AgentRegistry;

/// Default pending-approval timeout (WI-16): a request no client answers
/// within this window is denied (`rejected` + `approval/timed-out` event).
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// How many recent resolutions to remember for late-responder arbitration.
const RESOLUTION_HISTORY: usize = 256;

/// Decision arriving from the UI channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow { remember: bool },
    Reject,
    Cancel,
}

impl ApprovalDecision {
    pub fn to_outcome(self) -> ApprovalOutcome {
        match self {
            ApprovalDecision::Allow { remember: true } => ApprovalOutcome::AllowedSimilar,
            ApprovalDecision::Allow { remember: false } => ApprovalOutcome::AllowedOnce,
            ApprovalDecision::Reject => ApprovalOutcome::Rejected,
            ApprovalDecision::Cancel => ApprovalOutcome::Cancelled,
        }
    }

    /// UI-channel tag for the protocol surface.
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalDecision::Allow { remember: true } => "allow-remember",
            ApprovalDecision::Allow { remember: false } => "allow",
            ApprovalDecision::Reject => "reject",
            ApprovalDecision::Cancel => "cancel",
        }
    }
}

struct Pending {
    tx: oneshot::Sender<ApprovalDecision>,
}

pub struct ApprovalBridge {
    registry: Arc<dyn AgentRegistry>,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    session_latest: Arc<Mutex<HashMap<String, String>>>,
    /// Pending-approval timeout; `None` disables it (WI-16). Defaults to
    /// [`DEFAULT_APPROVAL_TIMEOUT`].
    timeout: Arc<Mutex<Option<Duration>>>,
    /// Recently resolved requests (first-wins arbitration record, WI-16):
    /// lets a late second responder learn the final decision.
    resolved: Arc<Mutex<VecDeque<(String, ApprovalDecision)>>>,
}

/// Await a pending decision with the WI-16 timeout: a request no client
/// answers in time is denied (`rejected`). Returns the outcome, the decision
/// when one arrived, and whether the timeout fired.
async fn await_decision(
    rx: oneshot::Receiver<ApprovalDecision>,
    timeout: Option<Duration>,
) -> (ApprovalOutcome, Option<ApprovalDecision>, bool) {
    match timeout {
        Some(limit) => match tokio::time::timeout(limit, rx).await {
            Ok(Ok(decision)) => (decision.to_outcome(), Some(decision), false),
            Ok(Err(_)) => (ApprovalOutcome::Cancelled, None, false),
            Err(_) => (ApprovalOutcome::Rejected, None, true),
        },
        None => match rx.await {
            Ok(decision) => (decision.to_outcome(), Some(decision), false),
            Err(_) => (ApprovalOutcome::Cancelled, None, false),
        },
    }
}

impl ApprovalBridge {
    pub fn new(registry: Arc<dyn AgentRegistry>) -> Self {
        Self::with_timeout(registry, Some(DEFAULT_APPROVAL_TIMEOUT))
    }

    /// Bridge with an explicit pending-request timeout (`None` = wait
    /// indefinitely, the pre-WI-16 behavior).
    pub fn with_timeout(registry: Arc<dyn AgentRegistry>, timeout: Option<Duration>) -> Self {
        Self {
            registry,
            pending: Arc::new(Mutex::new(HashMap::new())),
            session_latest: Arc::new(Mutex::new(HashMap::new())),
            timeout: Arc::new(Mutex::new(timeout)),
            resolved: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Retune the pending-request timeout (e.g. from a settings update).
    pub fn set_timeout(&self, timeout: Option<Duration>) {
        *self.timeout.lock().unwrap() = timeout;
    }

    /// The latest approval request id seen for a session (pending or
    /// already resolved).
    pub fn session_latest_request(&self, session_id: &str) -> Option<String> {
        self.session_latest.lock().unwrap().get(session_id).cloned()
    }

    /// Final decision recorded for an already-resolved request (WI-16
    /// multi-UI arbitration): the first responder wins; late responders can
    /// be told what actually happened instead of a bare "not pending".
    pub fn resolution_of(&self, request_id: &str) -> Option<ApprovalDecision> {
        self.resolved
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(id, _)| id == request_id)
            .map(|(_, decision)| *decision)
    }

    pub fn register(&self, bus: &EventBus) -> cordis_rslite::EffectHandle {
        let pending = Arc::clone(&self.pending);
        let session_latest = Arc::clone(&self.session_latest);
        let registry = Arc::clone(&self.registry);
        let timeout = Arc::clone(&self.timeout);
        bus.on_waterfall::<ApprovalRequestEvent, _>(move |event, _next| {
            let pending = Arc::clone(&pending);
            let session_latest = Arc::clone(&session_latest);
            let registry = Arc::clone(&registry);
            let timeout = Arc::clone(&timeout);
            Box::pin(async move {
                let req = event.req.clone();
                if req.signal.as_ref().is_some_and(|s| s.is_cancelled()) {
                    return Ok(ApprovalOutcome::Cancelled);
                }
                let Some(agent) = registry.get(&SessionId(req.agent_id.clone())) else {
                    return Ok(ApprovalOutcome::Unavailable);
                };
                // Find the newest unmatched approval/asked for this call
 // (-aligned: the audit pair is appended by the service).
                let events = agent.session().events();
                let claimed: std::collections::HashSet<String> =
                    pending.lock().unwrap().keys().cloned().collect();
                let mut decided = std::collections::HashSet::new();
                let mut approval_id = None;
                for event in events.iter().rev() {
                    match event.event_type.as_str() {
                        "approval/decided" => {
                            if let Some(id) = event.data.as_str_at("id") {
                                decided.insert(id.to_string());
                            }
                        }
                        "approval/asked" => {
                            let id = event.data.as_str_at("id").unwrap_or_default().to_string();
                            if decided.contains(&id) || claimed.contains(&id) {
                                continue;
                            }
                            // BUG-24: the audit pair belongs to THIS call —
                            // match the callId by value (both-absent still
                            // pairs), not by mere presence. A presence-only
                            // comparison let one call's request claim a
                            // different call's pending slot under parallel
                            // tool calls.
                            let event_call = event.data.get("callId").and_then(JsonValue::as_str);
                            let call_matches = match req.call_id.as_deref() {
                                Some(call) => event_call == Some(call),
                                None => event_call.is_none(),
                            };
                            if !call_matches {
                                continue;
                            }
                            approval_id = Some(id);
                            break;
                        }
                        _ => {}
                    }
                }
                let Some(id) = approval_id else {
                    return Ok(ApprovalOutcome::Unavailable);
                };
                let (tx, rx) = oneshot::channel();
                pending.lock().unwrap().insert(id.clone(), Pending { tx });
                session_latest
                    .lock()
                    .unwrap()
                    .insert(req.agent_id.clone(), id.clone());
                let limit = *timeout.lock().unwrap();
                let (outcome, _decision, timed_out) = await_decision(rx, limit).await;
                // The first-wins arbitration record is appended by `respond`
                // itself, so every resolution path (protocol client, legacy
                // approve, direct respond) is covered exactly once.
                pending.lock().unwrap().remove(&id);
                if timed_out {
                    // WI-16: the timeout denies the request; the service
                    // completes the audit pair (decided/rejected) and the
                    // timeout itself is an explicit session event.
                    let _ = agent.session().append(
                        "approval/timed-out",
                        JsonValue::Object(vec![
                            ("id".into(), JsonValue::String(id.clone())),
                            (
                                "timeoutMs".into(),
                                JsonValue::UInt(
                                    limit.map(|d| d.as_millis() as u64).unwrap_or_default(),
                                ),
                            ),
                        ]),
                    );
                }
                Ok(outcome)
            })
        })
    }

    /// Legacy entry: resolves the latest pending request for a session.
    pub fn approve(&self, session_id: &str, allowed: bool, remember: bool) -> bool {
        let id = self.session_latest.lock().unwrap().get(session_id).cloned();
        let Some(id) = id else { return false };
        let decision = if allowed {
            ApprovalDecision::Allow { remember }
        } else {
            ApprovalDecision::Reject
        };
        self.respond(&id, decision)
    }

    pub fn respond(&self, request_id: &str, decision: ApprovalDecision) -> bool {
        let Some(pending) = self.pending.lock().unwrap().remove(request_id) else {
            return false;
        };
        // First-wins arbitration record (bounded): recorded at the single
        // point where a decision wins, so late responders can be told what
        // actually happened regardless of which channel resolved the request.
        {
            let mut resolved = self.resolved.lock().unwrap();
            if resolved.len() >= RESOLUTION_HISTORY {
                resolved.pop_front();
            }
            resolved.push_back((request_id.to_string(), decision));
        }
        let _ = pending.tx.send(decision);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_maps_to_outcome() {
        assert_eq!(
            ApprovalDecision::Allow { remember: true }.to_outcome(),
            ApprovalOutcome::AllowedSimilar
        );
        assert_eq!(
            ApprovalDecision::Allow { remember: false }.to_outcome(),
            ApprovalOutcome::AllowedOnce
        );
        assert_eq!(
            ApprovalDecision::Reject.to_outcome(),
            ApprovalOutcome::Rejected
        );
        assert_eq!(
            ApprovalDecision::Cancel.to_outcome(),
            ApprovalOutcome::Cancelled
        );
    }

    #[test]
    fn respond_unknown_request_id_is_noop() {
        let registry: Arc<dyn AgentRegistry> =
            Arc::new(starriver_agent_loop::registry::InMemoryAgentRegistry::new());
        let bridge = ApprovalBridge::new(registry);
        assert!(!bridge.respond("missing", ApprovalDecision::Reject));
    }

    #[test]
    fn approve_without_pending_request_is_noop() {
        let registry: Arc<dyn AgentRegistry> =
            Arc::new(starriver_agent_loop::registry::InMemoryAgentRegistry::new());
        let bridge = ApprovalBridge::new(registry);
        assert!(!bridge.approve("s1", true, false));
    }

    fn empty_bridge() -> ApprovalBridge {
        let registry: Arc<dyn AgentRegistry> =
            Arc::new(starriver_agent_loop::registry::InMemoryAgentRegistry::new());
        ApprovalBridge::new(registry)
    }

    // ── WI-16: pending timeout ──

    #[tokio::test]
    async fn timeout_denies_and_flags() {
        let (_tx, rx) = oneshot::channel::<ApprovalDecision>();
        let (outcome, decision, timed_out) =
            await_decision(rx, Some(Duration::from_millis(20))).await;
        assert_eq!(outcome, ApprovalOutcome::Rejected);
        assert!(decision.is_none());
        assert!(timed_out);
    }

    #[tokio::test]
    async fn answer_before_timeout_wins() {
        let (tx, rx) = oneshot::channel::<ApprovalDecision>();
        tx.send(ApprovalDecision::Allow { remember: false })
            .unwrap();
        let (outcome, decision, timed_out) =
            await_decision(rx, Some(Duration::from_secs(30))).await;
        assert_eq!(outcome, ApprovalOutcome::AllowedOnce);
        assert_eq!(decision, Some(ApprovalDecision::Allow { remember: false }));
        assert!(!timed_out);
    }

    #[tokio::test]
    async fn dropped_channel_cancels() {
        let (tx, rx) = oneshot::channel::<ApprovalDecision>();
        drop(tx);
        let (outcome, decision, timed_out) =
            await_decision(rx, Some(Duration::from_secs(30))).await;
        assert_eq!(outcome, ApprovalOutcome::Cancelled);
        assert!(decision.is_none());
        assert!(!timed_out);
    }

    #[tokio::test]
    async fn disabled_timeout_waits_for_the_answer() {
        let (tx, rx) = oneshot::channel::<ApprovalDecision>();
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let _ = tx.send(ApprovalDecision::Reject);
        });
        let (outcome, _, timed_out) = await_decision(rx, None).await;
        task.await.unwrap();
        assert_eq!(outcome, ApprovalOutcome::Rejected);
        assert!(!timed_out);
    }

    #[test]
    fn timeout_is_configurable() {
        let bridge = empty_bridge();
        bridge.set_timeout(None);
        assert_eq!(*bridge.timeout.lock().unwrap(), None);
        bridge.set_timeout(Some(Duration::from_secs(1)));
        assert_eq!(
            *bridge.timeout.lock().unwrap(),
            Some(Duration::from_secs(1))
        );
    }

    // ── WI-16: multi-UI first-wins arbitration ──

    #[test]
    fn first_resolution_wins_and_is_remembered() {
        let bridge = empty_bridge();
        let (tx, rx) = oneshot::channel::<ApprovalDecision>();
        bridge
            .pending
            .lock()
            .unwrap()
            .insert("ap-1".into(), Pending { tx });
        assert!(bridge.respond("ap-1", ApprovalDecision::Allow { remember: true }));
        assert_eq!(
            rx.blocking_recv(),
            Ok(ApprovalDecision::Allow { remember: true })
        );
        // Second (late) responder loses…
        assert!(!bridge.respond("ap-1", ApprovalDecision::Reject));
        // …but can be told the final decision.
        assert_eq!(
            bridge.resolution_of("ap-1"),
            Some(ApprovalDecision::Allow { remember: true })
        );
        assert_eq!(bridge.resolution_of("unknown"), None);
    }

    // ── BUG-24: the audit-pair claim must match the callId VALUE ──

    mod bug24 {
        use super::*;
        use cordis_rslite::BoxFuture;
        use starriver_agent_loop::registry::InMemoryAgentRegistry;
        use starriver_kernel::agent::{
            Agent, AgentCancelCause, AgentOptions, AgentStatus, InboxTarget,
        };
        use starriver_kernel::llm::Message;
        use starriver_kernel::session::{Session, SessionId};

        struct StubAgent {
            id: SessionId,
            options: AgentOptions,
            session: Session,
            ctx: cordis_rslite::Context,
        }

        impl Agent for StubAgent {
            fn id(&self) -> &SessionId {
                &self.id
            }
            fn options(&self) -> &AgentOptions {
                &self.options
            }
            fn session(&self) -> &Session {
                &self.session
            }
            fn status(&self) -> AgentStatus {
                AgentStatus::Idle
            }
            fn ctx(&self) -> &cordis_rslite::Context {
                &self.ctx
            }
            fn followup(&self, _message: Message) {}
            fn steer(&self, _message: Message) {}
            fn inject(&self, _message: Message) {}
            fn send(&self, _message: Message, _target: InboxTarget, _wakeup: bool) {}
            fn cancel(&self, _cause: AgentCancelCause, _keep_inbox: bool) -> bool { false }
            fn when_idle(&self) -> BoxFuture<()> {
                Box::pin(async {})
            }
        }

        fn asked(id: &str, call_id: Option<&str>) -> JsonValue {
            let mut fields = vec![("id".into(), JsonValue::String(id.into()))];
            if let Some(call) = call_id {
                fields.push(("callId".into(), JsonValue::String(call.into())));
            }
            JsonValue::Object(fields)
        }

        /// A request for call-7 must not claim the pending audit pair of
        /// call-9: with parallel tool calls both pairs are unmatched at the
        /// same time, and the presence-only comparison grabs whichever asked
        /// event is newest — the UI answer then lands on the wrong call's
        /// pending slot.
        #[tokio::test]
        async fn request_claims_only_its_own_callid_audit_pair() {
            let bus = EventBus::new();
            let registry: Arc<dyn AgentRegistry> = Arc::new(InMemoryAgentRegistry::new());
            let agent = {
                let inner_bus = EventBus::new();
                let ctx = cordis_rslite::Context::new().with_event_bus(&inner_bus);
                let session = Session::new(SessionId("s1".into()), inner_bus);
                Arc::new(StubAgent {
                    id: SessionId("s1".into()),
                    options: AgentOptions::default(),
                    session,
                    ctx,
                })
            };
            registry.register(agent.clone()).unwrap();
            // A different call's still-unmatched audit pair.
            agent
                .session()
                .append("approval/asked", asked("ar-9", Some("call-9")))
                .unwrap();

            let bridge = ApprovalBridge::with_timeout(registry.clone(), Some(Duration::from_millis(50)));
            let _handle = bridge.register(&bus);

            let event = Arc::new(ApprovalRequestEvent {
                req: starriver_kernel::approval::ApprovalRequest {
                    agent_id: "s1".into(),
                    tool_name: "bash".into(),
                    call_id: Some("call-7".into()),
                    reason: None,
                    signal: None,
                    arguments: None,
                },
                signal: None,
            });
            let outcome = bus
                .waterfall_scoped(&cordis_rslite::Context::new(), event, || {
                    Box::pin(async { Ok(ApprovalOutcome::Unavailable) })
                })
                .await
                .unwrap();

            assert_eq!(
                outcome,
                ApprovalOutcome::Unavailable,
                "no audit pair for call-7 exists, so the request must go unanswered"
            );
            assert!(
                bridge.session_latest_request("s1").is_none(),
                "call-7's request must not claim call-9's pending slot"
            );
            let timed_out = agent
                .session()
                .events()
                .iter()
                .any(|e| e.event_type == "approval/timed-out");
            assert!(!timed_out, "an unanswered request must not time out ar-9");
        }
    }
}
