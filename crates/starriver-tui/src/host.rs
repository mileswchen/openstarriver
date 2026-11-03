use std::path::PathBuf;

use anyhow::Result;
use starriver_host::types::{
    AgentModelBinding, ChatMessage, ModelEntry, SessionInfo, SessionMessage, WorkspaceInfo,
};
use starriver_host::HostRuntime;

/// Type definitions exposed to the upper layer (including integration tests).
pub use starriver_host::types;

/// The app layer depends only on this trait, for terminal/DB-free unit tests.
pub trait HostApi {
    fn execute(
        &self,
        task: String,
        session_id: Option<String>,
        execution_mode: Option<String>,
        agent: Option<String>,
        isolation: Option<bool>,
    ) -> Result<tokio::sync::broadcast::Receiver<serde_json::Value>>;
    fn approve(&self, session_id: String, allowed: bool, remember: Option<bool>) -> Result<()>;
    fn cancel(&self, session_id: String) -> bool;
    fn list_sessions(&self, project_id: Option<String>) -> Vec<SessionInfo>;
    fn get_session_messages(&self, session_id: String) -> Vec<ChatMessage>;
    /// /delete: delete a session (child branches are promoted to root).
    fn delete_session(&self, session_id: String) -> Result<()>;
    /// /backtrack: session message timeline (with DB id, for locating the backtrack truncation point).
    fn get_session_timeline(&self, session_id: String) -> Vec<SessionMessage>;
    /// /backtrack: fork a new thread before before_msg_id, returning the new session info.
    fn fork_session(
        &self,
        source_id: String,
        before_msg_id: i64,
        task: String,
    ) -> Result<SessionInfo>;
    /// /name: name a session (Some sets an override title, None clears back to task-derived).
    fn rename_session(&self, session_id: String, title: Option<String>) -> Result<()>;
    /// /archive: soft-archive a session (hidden from the default list, restorable via /unarchive).
    fn archive_session(&self, session_id: String) -> Result<()>;
    /// /unarchive: restore an archived session.
    fn unarchive_session(&self, session_id: String) -> Result<()>;
    /// Data source for the /unarchive picker: archived sessions only.
    fn list_archived_sessions(&self) -> Vec<SessionInfo>;
    /// /review: start a code review (review-specific system prompt + read/grep whitelist).
    fn start_code_review(&self, files_json: String, dimensions_json: Option<String>) -> Result<()>;
    fn list_workspaces(&self) -> Vec<WorkspaceInfo>;
 /// semantics: register an existing directory (no mkdir, idempotent by
    /// path; the title defaults to the basename).
    fn create_workspace(&self, path: String) -> Result<WorkspaceInfo>;
    fn set_active_workspace_for(
        &self,
        agent_name: &str,
        workspace_id: Option<String>,
    ) -> Result<()>;
    fn get_active_workspace_for(&self, agent_name: &str) -> Option<String>;
    /// Actually configured models (model_registry / models.yaml), not built-in presets.
    fn list_models(&self) -> Vec<ModelEntry>;
    fn set_agent_active_model(
        &self,
        agent_name: String,
        provider_id: String,
        variant_id: String,
    ) -> Result<()>;
    fn list_agent_model_bindings(&self) -> Vec<AgentModelBinding>;
    fn shutdown(&self) -> Result<()>;
    /// /compact: compact a session, returning (before_tokens, after_tokens).
    fn compact_session(&self, session_id: String) -> Result<(usize, usize)>;
    /// /diff: workspace git diff text (empty string when no changes).
    fn workspace_diff(&self, cwd: String) -> Result<String>;
    /// /usage: session (token estimate, message count).
    fn session_usage(&self, session_id: String) -> Result<(usize, usize)>;
    /// /init: generate an AGENTS.md (does not overwrite if present), returning the path.
    fn init_project_agents(&self, cwd: String) -> Result<PathBuf>;
    /// /export: export a session to a Markdown file, returning the path.
    fn export_session(&self, session_id: String, cwd: String) -> Result<PathBuf>;
    /// /memory status: shared-store inventory JSON.
    fn memory_status(&self) -> Result<serde_json::Value>;
    /// /memory ingest: manual external-agent sweep, returning the report JSON.
    fn memory_ingest(&self) -> Result<serde_json::Value>;
    /// /memory export <dir>: write the marked section into AGENTS.md/CLAUDE.md.
    fn memory_export(&self, cwd: String) -> Result<serde_json::Value>;
    /// /preset: switch the global execution-mode preset ("standard"|"plan"|"ptc"|"goal");
    /// writes sandboxMode/approvalMode per the mode profile (settings + kernel push).
    fn set_execution_mode(&self, mode: String) -> Result<()>;
    /// /preset: per-session approval mode override ("manual"|"accept-edits"|"auto"|"bypass";
    /// appends an approval/policy session event; ByPass needs the governance toggle).
    fn set_approval_mode(&self, session_id: String, mode: String) -> Result<()>;
}

/// Real host: holds HostRuntime and forwards method by method.
pub struct Host {
    inner: HostRuntime,
}

impl Host {
    pub fn new(config_path: Option<String>) -> Result<Self> {
        Ok(Self {
            inner: HostRuntime::new(config_path)?,
        })
    }
}

impl HostApi for Host {
    fn execute(
        &self,
        task: String,
        session_id: Option<String>,
        execution_mode: Option<String>,
        agent: Option<String>,
        isolation: Option<bool>,
    ) -> Result<tokio::sync::broadcast::Receiver<serde_json::Value>> {
        self.inner.execute(
            task,
            session_id,
            execution_mode,
            agent,
            isolation,
            None,
            None,
        )
    }
    fn approve(&self, session_id: String, allowed: bool, remember: Option<bool>) -> Result<()> {
        // false = no pending approval / already responded (first come, first served); the caller does not block.
        let _ = self.inner.approve(session_id, allowed, remember);
        Ok(())
    }
    fn cancel(&self, session_id: String) -> bool {
        self.inner.cancel(session_id)
    }
    fn list_sessions(&self, project_id: Option<String>) -> Vec<SessionInfo> {
        self.inner.list_sessions(project_id)
    }
    fn get_session_messages(&self, session_id: String) -> Vec<ChatMessage> {
        self.inner.get_session_messages(session_id)
    }
    fn delete_session(&self, session_id: String) -> Result<()> {
        self.inner.delete_session(session_id)
    }
    fn get_session_timeline(&self, session_id: String) -> Vec<SessionMessage> {
        self.inner.get_session_timeline(session_id)
    }
    fn fork_session(
        &self,
        source_id: String,
        before_msg_id: i64,
        task: String,
    ) -> Result<SessionInfo> {
        self.inner.fork_session(source_id, before_msg_id, task)
    }
    fn rename_session(&self, session_id: String, title: Option<String>) -> Result<()> {
        self.inner.rename_session(session_id, title)
    }
    fn archive_session(&self, session_id: String) -> Result<()> {
        self.inner.archive_session(session_id)
    }
    fn unarchive_session(&self, session_id: String) -> Result<()> {
        self.inner.unarchive_session(session_id)
    }
    fn list_archived_sessions(&self) -> Vec<SessionInfo> {
        self.inner.list_archived_sessions()
    }
    fn start_code_review(&self, files_json: String, dimensions_json: Option<String>) -> Result<()> {
        self.inner.start_code_review(files_json, dimensions_json)
    }
    fn list_workspaces(&self) -> Vec<WorkspaceInfo> {
        self.inner.list_workspaces()
    }
    fn create_workspace(&self, path: String) -> Result<WorkspaceInfo> {
        self.inner.create_workspace(path)
    }
    fn set_active_workspace_for(
        &self,
        agent_name: &str,
        workspace_id: Option<String>,
    ) -> Result<()> {
        self.inner
            .set_active_workspace_for(agent_name, workspace_id)
    }
    fn get_active_workspace_for(&self, agent_name: &str) -> Option<String> {
        self.inner.get_active_workspace_for(agent_name)
    }
    fn list_models(&self) -> Vec<ModelEntry> {
        self.inner.list_models()
    }
    fn set_agent_active_model(
        &self,
        agent_name: String,
        provider_id: String,
        variant_id: String,
    ) -> Result<()> {
        self.inner
            .set_agent_active_model(agent_name, provider_id, variant_id)
    }
    fn list_agent_model_bindings(&self) -> Vec<AgentModelBinding> {
        self.inner.list_agent_model_bindings()
    }
    fn shutdown(&self) -> Result<()> {
        // HostRuntime::shutdown is async; host owns a tokio runtime internally.
        self.inner.runtime_handle().block_on(self.inner.shutdown())
    }
    fn compact_session(&self, session_id: String) -> Result<(usize, usize)> {
        let r = self.inner.compact_session(session_id, None)?;
        Ok((r.before as usize, r.after as usize))
    }
    fn workspace_diff(&self, cwd: String) -> Result<String> {
        self.inner.workspace_diff(&cwd)
    }
    fn session_usage(&self, session_id: String) -> Result<(usize, usize)> {
        self.inner.session_usage(&session_id)
    }
    fn init_project_agents(&self, cwd: String) -> Result<PathBuf> {
        self.inner.init_project_agents(&cwd)
    }
    fn export_session(&self, session_id: String, cwd: String) -> Result<PathBuf> {
        self.inner.export_session(&session_id, &cwd)
    }
    fn memory_status(&self) -> Result<serde_json::Value> {
        Ok(self.inner.memory_status())
    }
    fn memory_ingest(&self) -> Result<serde_json::Value> {
        Ok(self.inner.ingest_memories())
    }
    fn memory_export(&self, cwd: String) -> Result<serde_json::Value> {
        self.inner.export_memories(cwd)
    }
    fn set_execution_mode(&self, mode: String) -> Result<()> {
        self.inner.set_execution_mode(mode)
    }
    fn set_approval_mode(&self, session_id: String, mode: String) -> Result<()> {
        self.inner.set_approval_mode(session_id, mode)
    }
}

/// In-memory host for tests (also reused by integration tests; no stable API promise).
#[doc(hidden)]
pub mod mock {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone)]
    pub struct MockHost {
        /// Arc-shared internal state: the test's outer mock and the app's clone see the same records.
        pub sessions: Arc<Mutex<Vec<SessionInfo>>>,
        pub workspaces: Arc<Mutex<Vec<WorkspaceInfo>>>,
        pub messages: Arc<Mutex<Vec<ChatMessage>>>,
        /// /backtrack timeline data (returned by get_session_timeline)
        pub timeline: Arc<Mutex<Vec<SessionMessage>>>,
        /// fork call records: (source_id, before_msg_id)
        pub forked: Arc<Mutex<Vec<(String, i64)>>>,
        /// /name call records: (session_id, title)
        pub renamed: Arc<Mutex<Vec<(String, Option<String>)>>>,
        /// /archive call records: session_id
        pub archived: Arc<Mutex<Vec<String>>>,
        /// /unarchive call records: session_id
        pub unarchived: Arc<Mutex<Vec<String>>>,
        /// /unarchive picker data source
        pub archived_sessions: Arc<Mutex<Vec<SessionInfo>>>,
        /// /review call records: files_json
        pub review_calls: Arc<Mutex<Vec<String>>>,
        /// /worktree isolation toggle records
        pub isolation: Arc<Mutex<Vec<bool>>>,
        pub tx: Arc<Mutex<Option<tokio::sync::broadcast::Sender<serde_json::Value>>>>,
        pub active_workspace: Arc<Mutex<Option<String>>>,
        pub approved: Arc<Mutex<Vec<(String, bool, Option<bool>)>>>,
        pub cancelled: Arc<Mutex<Vec<String>>>,
        pub model_entries: Arc<Mutex<Vec<ModelEntry>>>,
        pub set_active: Arc<Mutex<Vec<(String, String, String)>>>,
        pub bindings: Arc<Mutex<Vec<AgentModelBinding>>>,
        /// execute call records: (task, session_id)
        pub executed: Arc<Mutex<Vec<(String, Option<String>)>>>,
        /// /compact /usage /diff /init /export call records (for assertions)
        pub compacted: Arc<Mutex<Vec<String>>>,
        pub usage_calls: Arc<Mutex<Vec<String>>>,
        pub diff_calls: Arc<Mutex<Vec<String>>>,
        /// Overridable /diff payload (tests seed a unified diff here).
        pub diff_output: Arc<Mutex<Option<String>>>,
        pub init_calls: Arc<Mutex<Vec<String>>>,
        pub export_calls: Arc<Mutex<Vec<(String, String)>>>,
        /// /preset: set_execution_mode call records (mode strings).
        pub execution_modes: Arc<Mutex<Vec<String>>>,
        /// /preset: set_approval_mode call records: (session_id, mode).
        pub approval_modes: Arc<Mutex<Vec<(String, String)>>>,
        /// Fault injection: when true, the preset setters fail (tests the error status path).
        pub fail_modes: Arc<Mutex<bool>>,
    }

    impl MockHost {
        /// Configure the event channel and return the sender, used by tests to inject events.
        pub fn with_events(&self, cap: usize) -> tokio::sync::broadcast::Sender<serde_json::Value> {
            let (tx, _rx) = tokio::sync::broadcast::channel(cap);
            *self.tx.lock().unwrap() = Some(tx.clone());
            tx
        }

        pub fn seed_session(&self, id: &str, title: &str) {
            self.sessions.lock().unwrap().push(SessionInfo {
                id: id.into(),
                title: title.into(),
                workspace_id: None,
                created_at: "2026-08-20".into(),
                message_count: 1,
                parent_id: None,
                fork_point: None,
                agent: None,
                origin: None,
            });
        }

        /// Override the /diff payload (WI-79 grouped rendering tests).
        pub fn set_diff_output(&self, text: &str) {
            *self.diff_output.lock().unwrap() = Some(text.to_string());
        }

        /// Seed the backtrack timeline (returned by get_session_timeline).
        pub fn seed_timeline(&self, msgs: Vec<SessionMessage>) {
            *self.timeline.lock().unwrap() = msgs;
        }

        /// Seed archived sessions (returned by list_archived_sessions).
        pub fn seed_archived_session(&self, id: &str, title: &str) {
            self.archived_sessions.lock().unwrap().push(SessionInfo {
                id: id.into(),
                title: title.into(),
                workspace_id: None,
                created_at: "2026-08-20".into(),
                message_count: 1,
                parent_id: None,
                fork_point: None,
                agent: None,
                origin: None,
            });
        }

        /// Register a workspace: path only — the title defaults to the path's
 /// basename, mirroring the resolve semantics.
        pub fn seed_workspace(&self, id: &str, path: &str) {
            let title = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path)
                .to_string();
            self.workspaces.lock().unwrap().push(WorkspaceInfo {
                id: id.into(),
                path: path.into(),
                title,
                created_at: "2026-08-20".into(),
                session_ids: vec![],
            });
        }

        pub fn seed_model_entry(&self, id: &str, name: &str, variants: &[&str]) {
            self.model_entries.lock().unwrap().push(ModelEntry {
                id: id.into(),
                name: name.into(),
                model_type: "cloud".into(),
                provider_type: "openai-compat".into(),
                base_url: format!("https://{id}.example.com"),
                api_key: "!keychain:test".into(),
                model_path: None,
                models: variants
                    .iter()
                    .map(|v| starriver_host::types::ModelVariant {
                        id: v.to_string(),
                        name: v.to_string(),
                        supports_vision: None,
                    })
                    .collect(),
                default_model: variants.first().map(|s| s.to_string()).unwrap_or_default(),
                concurrency: None,
                max_tokens: None,
                context_window: None,
                temperature: None,
                api_format: "openai-chat".into(),
                reasoning_effort: None,
                model_label: None,
                supports_vision: None,
            });
        }
    }

    impl HostApi for MockHost {
        fn execute(
            &self,
            task: String,
            session_id: Option<String>,
            _execution_mode: Option<String>,
            agent: Option<String>,
            isolation: Option<bool>,
        ) -> Result<tokio::sync::broadcast::Receiver<serde_json::Value>> {
            self.executed.lock().unwrap().push((task, session_id));
            let _ = (agent, isolation);
            let tx = self
                .tx
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no event channel configured"))?;
            Ok(tx.subscribe())
        }
        fn approve(&self, session_id: String, allowed: bool, remember: Option<bool>) -> Result<()> {
            self.approved
                .lock()
                .unwrap()
                .push((session_id, allowed, remember));
            Ok(())
        }
        fn cancel(&self, session_id: String) -> bool {
            self.cancelled.lock().unwrap().push(session_id);
            true
        }
        fn list_sessions(&self, _project_id: Option<String>) -> Vec<SessionInfo> {
            self.sessions.lock().unwrap().clone()
        }
        fn get_session_messages(&self, _session_id: String) -> Vec<ChatMessage> {
            self.messages.lock().unwrap().clone()
        }
        fn delete_session(&self, session_id: String) -> Result<()> {
            self.sessions.lock().unwrap().retain(|s| s.id != session_id);
            Ok(())
        }
        fn get_session_timeline(&self, _session_id: String) -> Vec<SessionMessage> {
            self.timeline.lock().unwrap().clone()
        }
        fn fork_session(
            &self,
            source_id: String,
            before_msg_id: i64,
            task: String,
        ) -> Result<SessionInfo> {
            self.forked
                .lock()
                .unwrap()
                .push((source_id.clone(), before_msg_id));
            let n = self.sessions.lock().unwrap().len() + 1;
            let info = SessionInfo {
                id: format!("fork-{n}"),
                title: task,
                workspace_id: None,
                created_at: "2026-08-20".into(),
                message_count: 0,
                parent_id: Some(source_id),
                fork_point: Some(before_msg_id),
                agent: None,
                origin: None,
            };
            self.sessions.lock().unwrap().push(info.clone());
            Ok(info)
        }
        fn rename_session(&self, session_id: String, title: Option<String>) -> Result<()> {
            self.renamed
                .lock()
                .unwrap()
                .push((session_id.clone(), title.clone()));
            if let Some(t) = title {
                if let Some(s) = self
                    .sessions
                    .lock()
                    .unwrap()
                    .iter_mut()
                    .find(|s| s.id == session_id)
                {
                    s.title = t;
                }
            }
            Ok(())
        }
        fn archive_session(&self, session_id: String) -> Result<()> {
            self.archived.lock().unwrap().push(session_id.clone());
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(idx) = sessions.iter().position(|s| s.id == session_id) {
                let s = sessions.remove(idx);
                self.archived_sessions.lock().unwrap().push(s);
            }
            Ok(())
        }
        fn unarchive_session(&self, session_id: String) -> Result<()> {
            self.unarchived.lock().unwrap().push(session_id.clone());
            let mut archived = self.archived_sessions.lock().unwrap();
            if let Some(idx) = archived.iter().position(|s| s.id == session_id) {
                let s = archived.remove(idx);
                self.sessions.lock().unwrap().push(s);
            }
            Ok(())
        }
        fn list_archived_sessions(&self) -> Vec<SessionInfo> {
            self.archived_sessions.lock().unwrap().clone()
        }
        fn start_code_review(
            &self,
            files_json: String,
            _dimensions_json: Option<String>,
        ) -> Result<()> {
            self.review_calls.lock().unwrap().push(files_json);
            Ok(())
        }
        fn list_workspaces(&self) -> Vec<WorkspaceInfo> {
            self.workspaces.lock().unwrap().clone()
        }
 /// semantics: resolve an existing directory (no mkdir); title
        /// defaults to the basename. A missing path is an error, and resolving
        /// the same path twice is idempotent.
        fn create_workspace(&self, path: String) -> Result<WorkspaceInfo> {
            let metadata = std::fs::metadata(&path)
                .map_err(|e| anyhow::anyhow!("workspace path must exist: {e}"))?;
            if !metadata.is_dir() {
                anyhow::bail!("workspace path must be a directory");
            }
            let title = std::path::Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path.as_str())
                .to_string();
            let mut workspaces = self.workspaces.lock().unwrap();
            if let Some(existing) = workspaces.iter().find(|w| w.path == path) {
                return Ok(existing.clone());
            }
            let w = WorkspaceInfo {
                id: format!("w{}", workspaces.len() + 1),
                path: path.clone(),
                title,
                created_at: "2026-08-20".into(),
                session_ids: vec![],
            };
            workspaces.push(w.clone());
            Ok(w)
        }
        fn set_active_workspace_for(
            &self,
            _agent_name: &str,
            workspace_id: Option<String>,
        ) -> Result<()> {
            *self.active_workspace.lock().unwrap() = workspace_id;
            Ok(())
        }
        fn get_active_workspace_for(&self, _agent_name: &str) -> Option<String> {
            self.active_workspace.lock().unwrap().clone()
        }
        fn list_models(&self) -> Vec<ModelEntry> {
            self.model_entries.lock().unwrap().clone()
        }
        fn set_agent_active_model(
            &self,
            agent_name: String,
            provider_id: String,
            variant_id: String,
        ) -> Result<()> {
            self.set_active
                .lock()
                .unwrap()
                .push((agent_name, provider_id, variant_id));
            Ok(())
        }
        fn list_agent_model_bindings(&self) -> Vec<AgentModelBinding> {
            self.bindings.lock().unwrap().clone()
        }
        fn shutdown(&self) -> Result<()> {
            Ok(())
        }
        fn compact_session(&self, session_id: String) -> Result<(usize, usize)> {
            self.compacted.lock().unwrap().push(session_id);
            Ok((120, 40))
        }
        fn workspace_diff(&self, cwd: String) -> Result<String> {
            self.diff_calls.lock().unwrap().push(cwd.clone());
            if let Some(text) = self.diff_output.lock().unwrap().clone() {
                return Ok(text);
            }
            if cwd.contains("norepo") {
                anyhow::bail!("not a git repository: {cwd}");
            }
            Ok("diff --git a/f.rs b/f.rs\n+hello world\n".into())
        }
        fn memory_status(&self) -> Result<serde_json::Value> {
            Ok(serde_json::json!({ "projects": [], "sources": [] }))
        }
        fn memory_ingest(&self) -> Result<serde_json::Value> {
            Ok(serde_json::json!({ "totalMemories": 0, "sources": [] }))
        }
        fn memory_export(&self, _cwd: String) -> Result<serde_json::Value> {
            Ok(serde_json::json!({ "written": [] }))
        }
        fn session_usage(&self, session_id: String) -> Result<(usize, usize)> {
            self.usage_calls.lock().unwrap().push(session_id);
            Ok((500, 3))
        }
        fn init_project_agents(&self, cwd: String) -> Result<PathBuf> {
            self.init_calls.lock().unwrap().push(cwd.clone());
            Ok(PathBuf::from(cwd).join("AGENTS.md"))
        }
        fn export_session(&self, session_id: String, cwd: String) -> Result<PathBuf> {
            self.export_calls
                .lock()
                .unwrap()
                .push((session_id.clone(), cwd.clone()));
            Ok(PathBuf::from(cwd).join(format!("starriver-export-{session_id}.md")))
        }
        fn set_execution_mode(&self, mode: String) -> Result<()> {
            if *self.fail_modes.lock().unwrap() {
                anyhow::bail!("mock: execution mode switch failed");
            }
            self.execution_modes.lock().unwrap().push(mode);
            Ok(())
        }
        fn set_approval_mode(&self, session_id: String, mode: String) -> Result<()> {
            if *self.fail_modes.lock().unwrap() {
                anyhow::bail!("mock: approval mode switch failed");
            }
            self.approval_modes.lock().unwrap().push((session_id, mode));
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        #[test]
        fn mock_execute_receives_events() {
            let mock = MockHost::default();
            let tx = mock.with_events(16);
            let mut rx = mock.execute("t".into(), None, None, None, None).unwrap();
            tx.send(json!({"type": "TextDone"})).unwrap();
            let v = rx.try_recv().unwrap();
            assert_eq!(v["type"], "TextDone");
        }

        #[test]
        fn mock_records_approve_and_cancel() {
            let mock = MockHost::default();
            mock.approve("s1".into(), true, Some(true)).unwrap();
            mock.cancel("s1".into());
            assert_eq!(mock.approved.lock().unwrap().len(), 1);
            assert_eq!(*mock.cancelled.lock().unwrap(), vec!["s1".to_string()]);
        }

        #[test]
        fn delete_session_removes_seeded() {
            let mock = MockHost::default();
            mock.seed_session("s1", "a");
            mock.seed_session("s2", "b");
            mock.delete_session("s1".into()).unwrap();
            let ids: Vec<String> = mock
                .sessions
                .lock()
                .unwrap()
                .iter()
                .map(|s| s.id.clone())
                .collect();
            assert_eq!(ids, vec!["s2".to_string()]);
        }

        #[test]
        fn create_workspace_resolves_existing_dir_with_basename_title() {
            let dir = tempfile::tempdir().expect("tempdir");
            let mock = MockHost::default();
            let path = dir.path().join("proj");
            std::fs::create_dir(&path).expect("dir");
            let p1 = mock
                .create_workspace(path.to_string_lossy().into_owned())
                .unwrap();
            assert_eq!(p1.title, "proj");
            // Same path resolves to the same workspace (idempotent).
            let p2 = mock
                .create_workspace(path.to_string_lossy().into_owned())
                .unwrap();
            assert_eq!(p1.id, p2.id);
            assert_eq!(mock.workspaces.lock().unwrap().len(), 1);
            assert!(mock
                .create_workspace("/nonexistent-dir-xyz".into())
                .is_err());
        }
        #[test]
        fn execute_records_task_and_session() {
            let mock = MockHost::default();
            mock.with_events(16);
            mock.execute("task".into(), Some("s9".into()), None, None, None)
                .unwrap();
            mock.execute("task2".into(), None, None, None, None)
                .unwrap();
            let calls = mock.executed.lock().unwrap();
            assert_eq!(calls[0], ("task".to_string(), Some("s9".to_string())));
            assert_eq!(calls[1], ("task2".to_string(), None));
        }
    }
}
