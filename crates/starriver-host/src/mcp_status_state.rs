//! Host-side shared MCP availability state (WI-163). The runtime records
//! every configured server whose tools are NOT currently registered —
//! discovery failed at startup, retries keep failing, or supervision marked
//! the connection dead. Per-run agent options read this through the plugin
//! context (`ctx.require`) so the agent-loop can tell the model what is
//! missing instead of letting it hallucinate around silent capability loss.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Context-provided handle; the runtime holds the write side, per-run
/// options hold readers.
#[derive(Clone, Default)]
pub struct McpUnavailableServers(pub Arc<RwLock<HashMap<String, String>>>);

impl cordis_rslite::TypedService for McpUnavailableServers {
    const NAME: &'static str = "seam:mcp-availability";
}

impl McpUnavailableServers {
    /// Sorted `(server, detail)` snapshot for the status feed.
    pub fn snapshot(&self) -> Vec<(String, String)> {
        let guard = self.0.read().unwrap();
        let mut list: Vec<(String, String)> = guard
            .iter()
            .map(|(server, detail)| (server.clone(), detail.clone()))
            .collect();
        list.sort();
        list
    }

    /// Record/refresh a server's failure detail.
    pub fn mark(&self, server: &str, detail: String) {
        self.0.write().unwrap().insert(server.to_string(), detail);
    }

    /// Clear a server after successful (re-)registration.
    pub fn clear(&self, server: &str) {
        self.0.write().unwrap().remove(server);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_sorted_and_marks_clear_roundtrip() {
        let state = McpUnavailableServers::default();
        state.mark("nexplan", "refused".into());
        state.mark("devops", "timeout".into());
        assert_eq!(
            state.snapshot(),
            vec![
                ("devops".to_string(), "timeout".to_string()),
                ("nexplan".to_string(), "refused".to_string()),
            ]
        );
        state.clear("nexplan");
        assert_eq!(state.snapshot().len(), 1);
    }
}
