//! Plugin platform P2: the plugin-task execution primitive (`pluginTask.*`).
//!
//! "Sessionless" here means **sessionless on every chat surface**, not
//! sessionless in storage: a plugin task still runs on the session engine
//! (agent loop, approvals, audit log and resume all hang off sessions), but
//! its internal task session is stamped `plugin:<name>` and excluded from
//! `session.list` (default), `session.listArchived`, `sessions.running` and
//! the sidebar. The `runId` handle returned by [`HostRuntime::plugin_task_run`]
//! is the only access path, and every method ownership-checks it against the
//! calling plugin's origin — a plugin can never read, steer or cancel another
//! plugin's runs or a user chat.
//!
//! Audit is explicit, never silent: runs are retained (no auto-delete, no
//! auto-archive) and enumerable via [`HostRuntime::plugin_task_list`] or
//! `session.list?origin=plugin:<name>`.
//!
//! The facade is synchronous and meant to be called from the protocol
//! server's blocking pool (like every other catalog handler).

use crate::runtime::WaitTurnOutcome;
use crate::types::{ChatMessage, SessionInfo};
use crate::HostRuntime;
use serde_json::Value;
use tokio::sync::broadcast::Receiver;

/// Typed facade errors so the protocol layer can map stable wire codes
/// (FORBIDDEN / NOT_FOUND / INVALID_PARAMS / SESSION_BUSY / INTERNAL)
/// instead of parsing message strings.
#[derive(Debug)]
pub enum PluginTaskError {
    /// The run exists but belongs to another plugin or a user chat.
    Forbidden(String),
    /// No such run handle.
    NotFound(String),
    /// Malformed plugin identity or parameters.
    InvalidParams(String),
    /// Underlying host failure (busy turn, model error, ...). The protocol
    /// layer applies the same `SESSION_BUSY:` prefix split as `session.execute`.
    Host(anyhow::Error),
}

impl std::fmt::Display for PluginTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forbidden(m) | Self::NotFound(m) | Self::InvalidParams(m) => f.write_str(m),
            Self::Host(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PluginTaskError {}

impl From<anyhow::Error> for PluginTaskError {
    fn from(e: anyhow::Error) -> Self {
        Self::Host(e)
    }
}

/// Same shape rule as the `session.execute` origin validation (P1): the
/// plugin namespace only, `[a-zA-Z0-9._-]{1,128}` after the prefix.
pub(crate) fn plugin_task_origin(plugin: &str) -> Result<String, PluginTaskError> {
    let valid = !plugin.is_empty()
        && plugin.len() <= 128
        && plugin
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid {
        return Err(PluginTaskError::InvalidParams(format!(
            "invalid plugin identity `{plugin}`"
        )));
    }
    Ok(format!("plugin:{plugin}"))
}

impl HostRuntime {
    /// Ownership gate: the run's durable origin stamp must equal exactly
    /// this plugin's origin. A row with a different origin (another plugin,
    /// a scheduled run, a user chat) is FORBIDDEN — not NOT_FOUND — because
    /// the handle itself is valid; leaking existence across owners is fine,
    /// leaking access is not.
    fn plugin_task_owned(&self, plugin: &str, run_id: &str) -> Result<(), PluginTaskError> {
        let origin = plugin_task_origin(plugin)?;
        let inner = self.inner.blocking_lock();
        let runtime = inner.as_ref().ok_or_else(|| {
            PluginTaskError::Host(anyhow::anyhow!("runtime not initialized"))
        })?;
        match runtime.host_store.get(run_id) {
            Some(row) if row.origin.as_deref() == Some(origin.as_str()) => Ok(()),
            Some(_) => Err(PluginTaskError::Forbidden(format!(
                "run `{run_id}` is not owned by plugin `{plugin}`"
            ))),
            None => Err(PluginTaskError::NotFound(format!("unknown run `{run_id}`"))),
        }
    }

    /// Start (or continue) a plugin task. Returns the run handle plus the
    /// per-run event broadcast; the protocol layer relays it as a
    /// subscription. A fresh run's origin is stamped **synchronously before
    /// execute** (the stamp is an upsert), so ownership checks never race the
    /// async projection task — execute's own creation-time stamp then sees
    /// the row and no-ops, while its unconditional agent stamp still lands.
    #[allow(clippy::too_many_arguments)]
    pub fn plugin_task_run(
        &self,
        plugin: String,
        task: String,
        run_id: Option<String>,
        permission_preset: Option<String>,
        agent: Option<String>,
        agent_preset: Option<String>,
        client_request_id: Option<String>,
    ) -> Result<(String, Receiver<Value>), PluginTaskError> {
        let origin = plugin_task_origin(&plugin)?;
        if task.trim().is_empty() {
            return Err(PluginTaskError::InvalidParams("task must not be empty".into()));
        }
        let run_id = match run_id {
            Some(id) => {
                // Continuation: the durable stamp is the authority.
                self.plugin_task_owned(&plugin, &id)?;
                id
            }
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                let inner = self.inner.blocking_lock();
                let runtime = inner.as_ref().ok_or_else(|| {
                    PluginTaskError::Host(anyhow::anyhow!("runtime not initialized"))
                })?;
                runtime
                    .host_store
                    .stamp_session_origin(&id, &origin)
                    .map_err(|e| {
                        PluginTaskError::Host(anyhow::anyhow!("stamp task origin failed: {e}"))
                    })?;
                id
            }
        };
        let receiver = self.execute(
            task,
            Some(run_id.clone()),
            None,
            agent,
            None,
            agent_preset,
            permission_preset,
            client_request_id,
            Some(origin),
        )?;
        Ok((run_id, receiver))
    }

    /// Block until the run's turn ends (or the deadline passes). Same
    /// semantics as `session.waitTurn`, ownership-gated.
    pub fn plugin_task_wait(
        &self,
        plugin: String,
        run_id: String,
        timeout_ms: Option<u64>,
    ) -> Result<WaitTurnOutcome, PluginTaskError> {
        self.plugin_task_owned(&plugin, &run_id)?;
        // Default 300s, hard cap 1h — mirrors session.waitTurn.
        let ms = timeout_ms.unwrap_or(300_000).min(3_600_000);
        Ok(self.wait_turn(&run_id, std::time::Duration::from_millis(ms)))
    }

    /// Read the run's message history. Always the paged shape
    /// `(messages, total, has_more)`; without `limit`, full history.
    pub fn plugin_task_messages(
        &self,
        plugin: String,
        run_id: String,
        limit: Option<usize>,
        before: Option<u64>,
    ) -> Result<(Vec<ChatMessage>, usize, bool), PluginTaskError> {
        self.plugin_task_owned(&plugin, &run_id)?;
        Ok(match limit {
            Some(limit) => self.get_session_messages_paged(run_id, before, limit.max(1)),
            None => {
                let messages = self.get_session_messages(run_id);
                let total = messages.len();
                (messages, total, false)
            }
        })
    }

    /// Cancel the run's active turn. `false` when nothing was running.
    pub fn plugin_task_cancel(
        &self,
        plugin: String,
        run_id: String,
    ) -> Result<bool, PluginTaskError> {
        self.plugin_task_owned(&plugin, &run_id)?;
        Ok(self.cancel(run_id))
    }

    /// Answer the run's pending approval request. `false` when no approval
    /// is pending (the protocol layer surfaces APPROVAL_NOT_PENDING).
    pub fn plugin_task_approve(
        &self,
        plugin: String,
        run_id: String,
        allowed: bool,
        remember: Option<bool>,
    ) -> Result<bool, PluginTaskError> {
        self.plugin_task_owned(&plugin, &run_id)?;
        Ok(self.approve(run_id, allowed, remember))
    }

    /// Audit list: every retained run owned by this plugin (newest first,
    /// the same ordering as `session.list`). Explicit enumeration — the
    /// chat surfaces never show these.
    pub fn plugin_task_list(&self, plugin: String) -> Result<Vec<SessionInfo>, PluginTaskError> {
        let origin = plugin_task_origin(&plugin)?;
        Ok(self
            .list_sessions(None)
            .into_iter()
            .filter(|s| s.origin.as_deref() == Some(origin.as_str()))
            .collect())
    }

    /// Chat-surface visibility predicate: whether a session id belongs to
    /// ANY plugin's task runs. `sessions.running` filters with this so the
    /// sidebar's running-poll never sees hidden runs (otherwise it would
    /// refresh the session list every 3s chasing a row that never appears).
    pub fn is_plugin_task_session(&self, session_id: &str) -> bool {
        let inner = self.inner.blocking_lock();
        inner.as_ref().is_some_and(|runtime| {
            runtime
                .host_store
                .get(session_id)
                .and_then(|row| row.origin)
                .is_some_and(|origin| origin.starts_with("plugin:"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_shape_matches_execute_validation() {
        assert_eq!(plugin_task_origin("starpaper").unwrap(), "plugin:starpaper");
        assert!(plugin_task_origin("a.b_c-d123").is_ok());
        assert!(plugin_task_origin("").is_err());
        assert!(plugin_task_origin("has space").is_err());
        assert!(plugin_task_origin("斜杠/名").is_err());
        assert!(plugin_task_origin(&"x".repeat(129)).is_err());
        assert!(plugin_task_origin(&"x".repeat(128)).is_ok());
    }

    #[test]
    fn error_display_is_message_only() {
        let e = PluginTaskError::Forbidden("run `r` is not owned by plugin `p`".into());
        assert_eq!(e.to_string(), "run `r` is not owned by plugin `p`");
        let e = PluginTaskError::Host(anyhow::anyhow!("SESSION_BUSY: r"));
        assert_eq!(e.to_string(), "SESSION_BUSY: r");
    }
}