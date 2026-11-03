//! Host-side approval answerer: listens on `approval/request`, resolves the
//! pending oneshot when the UI responds. UI-agnostic: any channel can call
//! `respond` / the legacy `approve` entry.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use cordis_rslite::EventBus;
use starriver_kernel::approval::{ApprovalOutcome, ApprovalRequestEvent};
use starriver_kernel::json::JsonValue;
use starriver_kernel::session::SessionId;
use tokio::sync::oneshot;

use starriver_kernel::agent::AgentRegistry;

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
    /// Recently resolved requests (first-wins arbitration record, WI-16):
    /// lets a late second responder learn the final decision.
    resolved: Arc<Mutex<VecDeque<(String, ApprovalDecision)>>>,
}

/// Await a pending decision. Approvals wait indefinitely (dsh/codex
/// parity): only an actual decision or the client channel dropping
/// (requester gone) settles the request — never a clock.
async fn await_decision(rx: oneshot::Receiver<ApprovalDecision>) -> ApprovalOutcome {
    match rx.await {
        Ok(decision) => decision.to_outcome(),
        Err(_) => ApprovalOutcome::Cancelled,
    }
}

impl ApprovalBridge {
    pub fn new(registry: Arc<dyn AgentRegistry>) -> Self {
        Self {
            registry,
            pending: Arc::new(Mutex::new(HashMap::new())),
            session_latest: Arc::new(Mutex::new(HashMap::new())),
            resolved: Arc::new(Mutex::new(VecDeque::new())),
        }
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
        bus.on_waterfall::<ApprovalRequestEvent, _>(move |event, _next| {
            let pending = Arc::clone(&pending);
            let session_latest = Arc::clone(&session_latest);
            let registry = Arc::clone(&registry);
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
                let outcome = await_decision(rx).await;
                // The first-wins arbitration record is appended by `respond`
                // itself, so every resolution path (protocol client, legacy
                // approve, direct respond) is covered exactly once.
                pending.lock().unwrap().remove(&id);
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

    // ── wait semantics: a decision wins, a dropped channel cancels, no clock ──

    #[tokio::test]
    async fn answer_wins() {
        let (tx, rx) = oneshot::channel::<ApprovalDecision>();
        tx.send(ApprovalDecision::Allow { remember: false })
            .unwrap();
        let outcome = await_decision(rx).await;
        assert_eq!(outcome, ApprovalOutcome::AllowedOnce);
    }

    #[tokio::test]
    async fn dropped_channel_cancels() {
        let (tx, rx) = oneshot::channel::<ApprovalDecision>();
        drop(tx);
        let outcome = await_decision(rx).await;
        assert_eq!(outcome, ApprovalOutcome::Cancelled);
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
            fn cancel(&self, _cause: AgentCancelCause, _keep_inbox: bool) -> bool {
                false
            }
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
            let _ = registry.register(agent.clone()).unwrap();
            // A different call's still-unmatched audit pair.
            agent
                .session()
                .append("approval/asked", asked("ar-9", Some("call-9")))
                .unwrap();

            let bridge = ApprovalBridge::new(registry.clone());
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
            let any_timed_out = agent
                .session()
                .events()
                .iter()
                .any(|e| e.event_type == "approval/timed-out");
            assert!(
                !any_timed_out,
                "approvals wait indefinitely — no timeout event may ever be appended"
            );
        }
    }
}
