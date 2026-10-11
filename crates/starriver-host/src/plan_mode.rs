//! Plan mode as logged collaboration state (plan fold, plan-mode parity).
//!
//! `plan/mode` (`{ active: bool }`) is a log-only, whole-value-replace
//! session event: durable and replayable, never in the model transcript.
//! [`effective_plan_mode`] is a pure fold of the session log (last event
//! wins, no event → false), so resume, fork, and compaction recover the
//! state with no live mirror. While active, the deployment-owned
//! `plan:policy` guidance section renders into each request
//! ([`PLAN_POLICY_SECTION`], wired in plugin_kernel at order 500).
//!
//! Legacy sessions predate the fold and carry `plan/phase` instead;
//! [`legacy_plan_phase_confirmed`] folds the old execute marker so a
//! confirmed session is never re-locked into planning.
//!
//! A selection made while a turn is open cannot append safely (the request
//! for the current step was already derived), so [`PlanModeController`]
//! records it process-locally and a per-agent `agent/pre-step` listener
//! lands it before the next request derivation — downstream listeners
//! first, only after they accept the step. A selection made after a turn's
//! final accepted pre-step stays pending until the next turn's first
//! pre-step.
//!
//! The ReadOnly sandbox stays hard-paired with the Plan profile (the
//! execution_mode preset stamps it; this module never enters plan mode on
//! its own) — except the approved exit flip, which bundles the
//! workspace-write release so tool calls in the very next step run unlocked.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cordis_rslite::EffectHandle;
use starriver_kernel::agent::{Agent, AgentPreStep, PreStepDecision};
use starriver_kernel::json::JsonValue;
use starriver_kernel::session::{Session, SessionError};

/// The logged plan-state event (`{ active: bool }`).
pub const PLAN_MODE_EVENT: &str = "plan/mode";

/// Guidance rendered as the `plan:policy` section while plan mode is active
/// (: deployment-owned soft guidance at first-party order 500; inactive
/// plan mode contributes no text).
pub const PLAN_POLICY_SECTION: &str = "\
## Plan Mode (active)

You are in plan mode: research and design only — do not modify anything.
- Read, search, and explore freely; write-capable actions are refused by the read-only sandbox.
- Produce a complete, concrete implementation plan: files to touch, exact changes, steps in order, and risks.
- When the plan is ready, call `exit_plan_mode` with the full plan as markdown (starting with a `#` heading) to present it for the user's review. Do not paste the final plan as a plain reply, and do not ask \"should I proceed?\" through prose or a question tool — only `exit_plan_mode` presents the plan for approval.
- A user's conversational agreement — including an answer confirming something you asked — approves nothing and does not end plan mode; fold the confirmed decision into the plan and submit it through `exit_plan_mode`.
- If the user rejects with feedback, revise the plan and present it again; do not start implementing.";

/// WI-178: standard (non-plan) mode guidance — the review loop works here
/// too; keep it short so it costs a few tokens per request, and scope it to
/// non-trivial work so trivial replies don't grow a plan round-trip.
pub const STANDARD_PLAN_HINT: &str = "\
## Plan review (optional, available)

For a non-trivial or multi-step change, present a complete implementation plan \
before touching anything: call `exit_plan_mode` with the full plan as markdown \
(starting with a `#` heading). The user approves — proceed exactly as planned — \
or rejects with feedback — revise and present again. Trivial one-step changes \
do not need a plan.";

/// Pure fold: the last `plan/mode` event in log order, `None` when the log
/// carries none (legacy sessions).
pub fn plan_mode_fold(session: &Session) -> Option<bool> {
    session
        .events()
        .into_iter()
        .rev()
        .find(|event| event.event_type == PLAN_MODE_EVENT)
        .and_then(|event| event.data.get("active").and_then(JsonValue::as_bool))
}

/// Legacy fold: the last `plan/phase` event marks the planning→execution
/// handoff of the pre-fold flow (`{ phase: "execute" }`).
pub fn legacy_plan_phase_confirmed(session: &Session) -> bool {
    session
        .events()
        .into_iter()
        .rev()
        .find(|event| event.event_type == "plan/phase")
        .and_then(|event| event.data.as_str_at("phase").map(String::from))
        .as_deref()
        == Some("execute")
}

/// The plan state in force: pure fold of the log. Legacy `plan/phase`
/// events never activate the guidance section; they only feed the entry
/// decision in the kernel's per-turn stamping.
pub fn effective_plan_mode(session: &Session) -> bool {
    plan_mode_fold(session).unwrap_or(false)
}

/// True while at least one `turn/start` lacks its matching `turn/end`
/// (same enclosure rule the approval service applies).
pub fn has_open_turn(session: &Session) -> bool {
    let mut depth = 0i64;
    for event in session.events() {
        match event.event_type.as_str() {
            "turn/start" => depth += 1,
            "turn/end" => depth -= 1,
            _ => {}
        }
    }
    depth > 0
}

/// What happened to a selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanSelectOutcome {
    /// Logged now (no open turn).
    Committed,
    /// Awaiting the next accepted in-turn pre-step.
    Queued,
    /// An opposite pending selection was cleared; the log already matches.
    Cancelled,
    /// Already in that state.
    Noop,
}

/// Owns the pending plan-state selection per session and lands it at the
/// next accepted in-turn pre-step. The kernel registers the listener on
/// every agent publication (the host disposes agents between turns, so each
/// execute gets a fresh context and a fresh registration).
pub struct PlanModeController {
    pending: Arc<Mutex<HashMap<String, bool>>>,
    /// Whether the Plan execution profile is active (the kernel's own lock,
    /// shared): the approved exit bundles the sandbox release only then.
    plan_profile: Arc<Mutex<Option<crate::execution_mode::ExecutionMode>>>,
}

impl PlanModeController {
    pub fn new(
        plan_profile: Arc<Mutex<Option<crate::execution_mode::ExecutionMode>>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            plan_profile,
        })
    }

    /// The logged state plus any selected state still waiting to be
    /// appended.
    pub fn get(&self, agent: &Arc<dyn Agent>) -> (bool, Option<bool>) {
        let logged = effective_plan_mode(agent.session());
        let pending = self
            .pending
            .lock()
            .unwrap()
            .get(agent.id().0.as_str())
            .copied();
        (logged, pending)
    }

    /// Select whether plan mode should be active. Between turns the change
    /// appends immediately (no in-turn pre-step will run until another turn
    /// starts); during an open turn it stays pending for the next accepted
    /// pre-step. Selecting the current or already-pending state is a no-op.
    pub fn select(&self, agent: &Arc<dyn Agent>, active: bool) -> PlanSelectOutcome {
        let session = agent.session();
        let key = agent.id().0.clone();
        // Bind the guard explicitly: an `if let` over a lock temporary would
        // hold it across the block and deadlock the re-lock below.
        let waiting = {
            let pending = self.pending.lock().unwrap();
            pending.get(&key).copied()
        };
        if let Some(waiting) = waiting {
            if waiting == active {
                return PlanSelectOutcome::Noop;
            }
            self.pending.lock().unwrap().remove(&key);
            // The "pending exists ⇒ log already matches `active`" invariant
            // breaks when other writers (confirm_plan, plugin entry/exit
            // flips) append plan/mode events directly — recheck before
            // discarding the user's request, or the explicit selection is
            // silently dropped.
            if effective_plan_mode(&session) == active {
                return PlanSelectOutcome::Cancelled;
            }
            // Fall through: the log does NOT match — commit/queue below.
        }
        if effective_plan_mode(&session) == active {
            return PlanSelectOutcome::Noop;
        }
        if has_open_turn(&session) {
            self.pending.lock().unwrap().insert(key, active);
            return PlanSelectOutcome::Queued;
        }
        if append_plan_mode(&session, active).is_ok() {
            PlanSelectOutcome::Committed
        } else {
            // Append failure keeps the selection for a later accepted
            // pre-step (same recovery rule as mid-turn); nothing blocks.
            self.pending.lock().unwrap().insert(key, active);
            PlanSelectOutcome::Queued
        }
    }

    /// Register the per-agent `agent/pre-step` listener that lands pending
    /// flips. Downstream listeners run first; the flip appends only after
 /// they accept the step and an append failure keeps the
    /// selection pending without blocking the turn.
    pub fn register_step_listener(&self, agent: &Arc<dyn Agent>) -> EffectHandle {
        let pending = Arc::clone(&self.pending);
        let plan_profile = Arc::clone(&self.plan_profile);
        let session = agent.session().clone();
        agent
            .ctx()
            .on_waterfall::<AgentPreStep, _>(move |_event, next| {
                let pending = Arc::clone(&pending);
                let plan_profile = Arc::clone(&plan_profile);
                let session = session.clone();
                Box::pin(async move {
                    let decision = next().await?;
                    if matches!(decision, PreStepDecision::Enter { .. }) {
                        let key = session.id.0.clone();
                        let selected = pending.lock().unwrap().remove(&key);
                        if let Some(active) = selected {
                            if append_plan_mode(&session, active).is_err() {
                                // Keep waiting for a later accepted pre-step.
                                pending.lock().unwrap().insert(key, active);
                            } else if !active
                                && plan_profile
                                    .lock()
                                    .unwrap()
                                    .is_some_and(crate::execution_mode::approval_locked)
                            {
                                // Approved exit under the Plan profile: release the
                                // read-only pairing in the same breath, or tool calls
                                // in the very next step would still run sandboxed.
                                let _ = session.append(
                                    "sandbox/mode",
                                    JsonValue::Object(vec![(
                                        "mode".into(),
                                        JsonValue::String("workspace-write".into()),
                                    )]),
                                );
                            }
                        }
                    }
                    Ok(decision)
                })
            })
    }
}

/// Log-only append of the whole-value-replace plan state (no surface op:
/// never in the model transcript, still visible to folds and event streams).
fn append_plan_mode(session: &Session, active: bool) -> Result<(), SessionError> {
    session
        .append(
            PLAN_MODE_EVENT,
            JsonValue::Object(vec![("active".into(), JsonValue::Bool(active))]),
        )
        .map(|_| ())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use cordis_rslite::EventBus;
    use starriver_kernel::session::SessionId;

    /// Minimal static-options agent shared with sibling test modules
    /// (`plan_review_bridge`) so registry `enter` works without a factory.
    pub(crate) struct FakeAgent {
        id: SessionId,
        session: Session,
        agent_ctx: cordis_rslite::Context,
    }

    impl FakeAgent {
        pub(crate) fn new(session: Session) -> std::sync::Arc<Self> {
            let bus = cordis_rslite::EventBus::new();
            let agent_ctx = cordis_rslite::Context::new().with_event_bus(&bus);
            std::sync::Arc::new(Self {
                id: session.id.clone(),
                session,
                agent_ctx,
            })
        }
    }

    impl Agent for FakeAgent {
        fn id(&self) -> &SessionId {
            &self.id
        }
        fn options(&self) -> &starriver_kernel::agent::AgentOptions {
            static OPTIONS: std::sync::OnceLock<starriver_kernel::agent::AgentOptions> =
                std::sync::OnceLock::new();
            OPTIONS.get_or_init(starriver_kernel::agent::AgentOptions::default)
        }
        fn session(&self) -> &Session {
            &self.session
        }
        fn status(&self) -> starriver_kernel::agent::AgentStatus {
            starriver_kernel::agent::AgentStatus::Idle
        }
        fn ctx(&self) -> &cordis_rslite::Context {
            &self.agent_ctx
        }
        fn followup(&self, _message: starriver_kernel::llm::Message) {}
        fn steer(&self, _message: starriver_kernel::llm::Message) {}
        fn inject(&self, _message: starriver_kernel::llm::Message) {}
        fn send(
            &self,
            _message: starriver_kernel::llm::Message,
            _target: starriver_kernel::agent::InboxTarget,
            _wakeup: bool,
        ) {
        }
        fn cancel(&self, _cause: starriver_kernel::agent::AgentCancelCause, _keep_inbox: bool) -> bool { false }
        fn when_idle(&self) -> cordis_rslite::BoxFuture<()> {
            Box::pin(async {})
        }
    }

    fn session_with(events: &[(&str, JsonValue)]) -> Session {
        let session = Session::new(SessionId("s1".into()), EventBus::new());
        for (kind, payload) in events {
            session.append(*kind, payload.clone()).unwrap();
        }
        session
    }

    fn plan_mode(active: bool) -> JsonValue {
        JsonValue::Object(vec![("active".into(), JsonValue::Bool(active))])
    }

    #[test]
    fn fold_is_last_wins_and_defaults_false() {
        let empty = session_with(&[]);
        assert_eq!(plan_mode_fold(&empty), None);
        assert!(!effective_plan_mode(&empty));

        let session = session_with(&[
            (PLAN_MODE_EVENT, plan_mode(true)),
            (PLAN_MODE_EVENT, plan_mode(false)),
        ]);
        assert_eq!(plan_mode_fold(&session), Some(false));
        assert!(!effective_plan_mode(&session));

        let active = session_with(&[(PLAN_MODE_EVENT, plan_mode(true))]);
        assert!(effective_plan_mode(&active));
    }

    #[test]
    fn legacy_phase_fold_only_feeds_the_entry_decision() {
        let confirmed = session_with(&[(
            "plan/phase",
            JsonValue::Object(vec![("phase".into(), JsonValue::String("execute".into()))]),
        )]);
        assert!(legacy_plan_phase_confirmed(&confirmed));
        assert!(
            !effective_plan_mode(&confirmed),
            "legacy execute never activates the section"
        );

        let planning = session_with(&[]);
        assert!(!legacy_plan_phase_confirmed(&planning));
    }

    #[test]
    fn open_turn_tracks_enclosure() {
        let open = session_with(&[("turn/start", JsonValue::UInt(1))]);
        assert!(has_open_turn(&open));
        let closed = session_with(&[
            ("turn/start", JsonValue::UInt(1)),
            ("turn/end", JsonValue::UInt(2)),
        ]);
        assert!(!has_open_turn(&closed));
    }

    // ── controller: select state machine + pre-step journaling ──

    fn live_agent(plan_active: bool) -> std::sync::Arc<dyn Agent> {
        let session = Session::new(SessionId("s-plan".into()), cordis_rslite::EventBus::new());
        if plan_active {
            session.append(PLAN_MODE_EVENT, plan_mode(true)).unwrap();
        }
        FakeAgent::new(session) as std::sync::Arc<dyn Agent>
    }

    fn controller() -> Arc<PlanModeController> {
        PlanModeController::new(Arc::new(std::sync::Mutex::new(Some(
            crate::execution_mode::ExecutionMode::Plan,
        ))))
    }

    #[test]
    fn select_commits_between_turns_and_noops_on_repeat() {
        let agent = live_agent(false);
        let controller = controller();
        assert_eq!(
            controller.select(&agent, true),
            PlanSelectOutcome::Committed
        );
        assert_eq!(controller.select(&agent, true), PlanSelectOutcome::Noop);
        assert_eq!(controller.get(&agent), (true, None));
        assert_eq!(
            controller.select(&agent, false),
            PlanSelectOutcome::Committed
        );
        assert!(!effective_plan_mode(agent.session()));
    }

    #[test]
    fn select_queues_during_open_turn_and_cancel_flips_pending() {
        let agent = live_agent(false);
        agent
            .session()
            .append("turn/start", JsonValue::UInt(1))
            .unwrap();
        let controller = controller();
        assert_eq!(controller.select(&agent, true), PlanSelectOutcome::Queued);
        assert_eq!(
            controller.get(&agent),
            (false, Some(true)),
            "not landed yet"
        );
        assert!(
            !agent
                .session()
                .events()
                .iter()
                .any(|e| e.event_type == PLAN_MODE_EVENT),
            "queued selection must not append before a pre-step accepts"
        );
        // Opposite selection before the flip lands: cleared, nothing appended.
        assert_eq!(
            controller.select(&agent, false),
            PlanSelectOutcome::Cancelled
        );
        assert_eq!(controller.get(&agent), (false, None));
    }

    #[test]
    fn pre_step_listener_lands_pending_exit_with_sandbox_release() {
        let agent = live_agent(true);
        // The read-only pairing of the planning phase.
        agent
            .session()
            .append(
                "sandbox/mode",
                JsonValue::Object(vec![("mode".into(), JsonValue::String("read-only".into()))]),
            )
            .unwrap();
        // Open turn: the exit lands queued (the exit_plan_mode approval path).
        agent
            .session()
            .append("turn/start", JsonValue::UInt(1))
            .unwrap();
        let controller = controller();
        assert_eq!(controller.select(&agent, false), PlanSelectOutcome::Queued);
        controller.register_step_listener(&agent);

        // The loop dispatches the pre-step waterfall scoped on the agent ctx;
        // fire it the same way and expect the flip + release afterwards.
        let event = Arc::new(AgentPreStep {
            turn: 1,
            step: 2,
            messages: Vec::new(),
            signal: tokio_util::sync::CancellationToken::new(),
        });
        let decision = futures::executor::block_on(agent.ctx().waterfall(event, || {
            Box::pin(async {
                Ok(PreStepDecision::Enter {
                    messages: Vec::new(),
                })
            })
                as cordis_rslite::BoxFuture<Result<PreStepDecision, cordis_rslite::EventError>>
        }))
        .unwrap();
        assert!(matches!(decision, PreStepDecision::Enter { .. }));
        let session = agent.session();
        assert!(!effective_plan_mode(&session), "exit flip landed");
        assert_eq!(
            starriver_kernel::sandbox::effective_sandbox_mode(&session),
            Some(starriver_kernel::sandbox::SandboxMode::WorkspaceWrite),
            "Plan-profile exit bundles the sandbox release"
        );
        assert_eq!(controller.get(&agent), (false, None), "pending consumed");
    }
}
