//! Host-side question answerer (WI-67, interaction · user-questions slice 1):
//! an agent run asks the user a question (`ask_user` tool), the question
//! surfaces as a UI modal through the same session-event → UiEvent chain as
//! approvals, and the answer resolves the pending oneshot. UI-agnostic: any
//! channel can call `respond`; no responder → timeout → the tool reports the
//! miss back to the model.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use starriver_kernel::agent::AgentRegistry;
use starriver_kernel::json::JsonValue;
use starriver_kernel::session::SessionId;
use tokio::sync::oneshot;

/// Default wait for a user answer; the tool call can override per question.
pub const DEFAULT_QUESTION_TIMEOUT: Duration = Duration::from_secs(120);

/// Why an ask ended without a user answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskError {
    Timeout,
    /// Session not found (e.g. already disposed when the answer came due).
    Unavailable,
}

impl AskError {
    /// Model-facing text: the loop turns tool content into the next turn's
    /// input, so the miss is reported as data the agent can adapt to.
    pub fn as_str(self) -> &'static str {
        match self {
            AskError::Timeout => "timeout",
            AskError::Unavailable => "unavailable",
        }
    }
}

struct PendingQuestion {
    tx: oneshot::Sender<String>,
}

pub struct QuestionBridge {
    registry: Arc<dyn AgentRegistry>,
    pending: Mutex<HashMap<String, PendingQuestion>>,
}

impl QuestionBridge {
    pub fn new(registry: Arc<dyn AgentRegistry>) -> Self {
        Self {
            registry,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Ask the user `question` (optional `options` the UI renders as buttons)
    /// and wait up to `timeout` for an answer. Appends the audit pair to the
    /// session log — `interaction/question` on ask, `interaction/answer` on
    /// resolve or abandonment — mirroring the approval audit trail.
    pub async fn ask(
        &self,
        session_id: &str,
        question: &str,
        options: &[String],
        timeout: Duration,
    ) -> Result<String, AskError> {
        let Some(agent) = self.registry.get(&SessionId(session_id.to_string())) else {
            return Err(AskError::Unavailable);
        };
        let session = agent.session();
        let question_id = uuid::Uuid::new_v4().to_string();
        let options_json: Vec<JsonValue> = options
            .iter()
            .map(|o| JsonValue::String(o.clone()))
            .collect();
        // Insert the pending slot BEFORE the question event becomes visible:
        // a fast UI that answers off the event must not race the slot insert.
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(question_id.clone(), PendingQuestion { tx });
        if session
            .append(
                "interaction/question",
                JsonValue::Object(vec![
                    ("id".into(), JsonValue::String(question_id.clone())),
                    ("text".into(), JsonValue::String(question.to_string())),
                    ("options".into(), JsonValue::Array(options_json)),
                    ("timeoutSecs".into(), JsonValue::UInt(timeout.as_secs())),
                ]),
            )
            .is_err()
        {
            self.pending.lock().unwrap().remove(&question_id);
            return Err(AskError::Unavailable);
        }
        // Only the ask path knows the session, so it records the resolution
        // event; `respond` itself stays session-free and just resolves.
        let answered = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) => Some(answer),
            _ => None,
        };
        self.pending.lock().unwrap().remove(&question_id);
        match answered {
            Some(answer) => {
                let _ = session.append(
                    "interaction/answer",
                    JsonValue::Object(vec![
                        ("id".into(), JsonValue::String(question_id)),
                        ("answer".into(), JsonValue::String(answer.clone())),
                    ]),
                );
                Ok(answer)
            }
            None => {
                let _ = session.append(
                    "interaction/answer",
                    JsonValue::Object(vec![
                        ("id".into(), JsonValue::String(question_id)),
                        ("abandoned".into(), JsonValue::Bool(true)),
                    ]),
                );
                Err(AskError::Timeout)
            }
        }
    }

    /// UI-channel answer: resolves the pending question. `false` = no such
    /// pending question; the answer event is recorded by the ask path.
    pub fn respond(&self, question_id: &str, answer: &str) -> bool {
        let tx = self
            .pending
            .lock()
            .unwrap()
            .remove(question_id)
            .map(|p| p.tx);
        match tx {
            Some(tx) => tx.send(answer.to_string()).is_ok(),
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_agent_loop::registry::InMemoryAgentRegistry;
    use starriver_kernel::agent::{AgentOptions, CreateAgentOptions};
    use starriver_kernel::session::SessionId;

    async fn bridge_with_session() -> (Arc<QuestionBridge>, Arc<InMemoryAgentRegistry>) {
        let registry = Arc::new(InMemoryAgentRegistry::new());
        registry
            .create(CreateAgentOptions {
                session_id: SessionId("s-q".into()),
                seed: vec![],
                cwd: None,
                agent_options: AgentOptions::default(),
                agent_preset: None,
            })
            .await
            .expect("create test agent");
        let bridge = Arc::new(QuestionBridge::new(
            Arc::clone(&registry) as Arc<dyn AgentRegistry>
        ));
        (bridge, registry)
    }

    fn question_id_of(registry: &InMemoryAgentRegistry) -> Option<String> {
        registry
            .get(&SessionId("s-q".into()))?
            .session()
            .events()
            .into_iter()
            .rev()
            .find(|e| e.event_type == "interaction/question")
            .and_then(|e| e.data.as_str_at("id").map(String::from))
    }

    #[tokio::test]
    async fn ask_unknown_session_is_unavailable() {
        let registry = Arc::new(InMemoryAgentRegistry::new());
        let bridge = QuestionBridge::new(Arc::clone(&registry) as Arc<dyn AgentRegistry>);
        let err = bridge
            .ask("no-such-session", "why?", &[], Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(err, AskError::Unavailable);
    }

    #[tokio::test]
    async fn respond_unknown_question_is_false() {
        let registry = Arc::new(InMemoryAgentRegistry::new());
        let bridge = QuestionBridge::new(Arc::clone(&registry) as Arc<dyn AgentRegistry>);
        assert!(!bridge.respond("missing-id", "42"));
    }
}
