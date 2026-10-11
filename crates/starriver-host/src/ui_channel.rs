//! UI channel contract: the narrow surface a UI needs to drive a session.
//! Management APIs (sessions/models/projects/agents) stay outside this trait;
//! a future web/mobile channel implements the same four methods.

use crate::approval_bridge::ApprovalDecision;
use crate::HostRuntime;

pub trait UiChannel: Send + Sync {
    fn execute(
        &self,
        task: String,
        session_id: Option<String>,
    ) -> anyhow::Result<tokio::sync::broadcast::Receiver<serde_json::Value>>;
    fn respond_approval(
        &self,
        request_id: String,
        allowed: bool,
        remember: bool,
    ) -> anyhow::Result<()>;
    fn cancel(&self, session_id: String) -> bool;
}

impl UiChannel for HostRuntime {
    fn execute(
        &self,
        task: String,
        session_id: Option<String>,
    ) -> anyhow::Result<tokio::sync::broadcast::Receiver<serde_json::Value>> {
        HostRuntime::execute(self, task, session_id, None, None, None, None, None)
    }

    fn respond_approval(
        &self,
        request_id: String,
        allowed: bool,
        remember: bool,
    ) -> anyhow::Result<()> {
        let inner = self.inner.blocking_lock();
        if let Some(inner) = inner.as_ref() {
            let decision = if allowed {
                ApprovalDecision::Allow { remember }
            } else {
                ApprovalDecision::Reject
            };
            inner.agent.approval_bridge.respond(&request_id, decision);
        }
        Ok(())
    }

    fn cancel(&self, session_id: String) -> bool {
        HostRuntime::cancel(self, session_id)
    }
}
