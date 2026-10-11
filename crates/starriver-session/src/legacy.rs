// P0-1 fix: SQLite WAL global single-write lock + batch commit + exponential backoff retry + periodic checkpoint
//
// Design points:
//   - global Arc<Mutex<Connection>> serializes all writes
//   - batch commit avoids per-row frequent commits
//   - automatic exponential backoff retry on database is locked (max 3 times)
//   - periodic lightweight checkpoint prevents unbounded WAL file growth

use rusqlite::OptionalExtension;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use starriver_kernel::config::SessionConfig;
use starriver_kernel::error::{AgentError, AgentResult};
use starriver_kernel::types::Message;

/// One recorded skill usage event (invocation / activation).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillUsageRecord {
    pub session_id: Option<String>,
    pub project_id: Option<String>,
    pub skill: String,
    pub version: Option<String>,
    /// Invocation mode: "command" (slash), "intent" (LLM match), "toggle" (manual).
    pub mode: String,
    pub ts: i64,
}

/// Session store — P0-1 SQLite global single-write lock
pub struct SessionStore {
    db: Arc<Mutex<rusqlite::Connection>>,
    config: SessionConfig,
}

/// Absolute path for the session DB. Absolute inputs pass through unchanged;
/// "~/..." expands to $HOME; any other relative input (e.g. the default
/// "./starriver.db") is anchored under ~/.starriver/ so the database never
/// depends on the process CWD.
fn resolve_session_db_path(db_path: &str) -> PathBuf {
    let p = Path::new(db_path);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    if let Some(rest) = db_path.strip_prefix("~/") {
        return home.join(rest);
    }
    home.join(".starriver").join(p)
}

impl SessionStore {
    /// Open or create database
    pub async fn open(config: &SessionConfig) -> AgentResult<Self> {
        // The session DB is user data. A relative db_path (the default is
        // "./starriver.db") resolves against the process CWD — which is "/" for
        // Finder/dock-launched apps and unwritable, so the backend constructor
        // failed on GUI launches while working from a terminal. Anchor relative
        // paths under ~/.starriver/ and make sure the parent dir exists.
        let db_path = resolve_session_db_path(&config.db_path);
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                AgentError::InternalError(anyhow::anyhow!(
                    "failed to create database dir {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let mut cfg = config.clone();
        cfg.db_path = db_path.to_string_lossy().into_owned();
        let conn = rusqlite::Connection::open(&db_path).map_err(|e| {
            AgentError::InternalError(anyhow::anyhow!("failed to open database: {e}"))
        })?;

        // WAL mode — supports concurrent reads
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             PRAGMA cache_size=-8000;
             PRAGMA foreign_keys=ON;",
        )
        .map_err(|e| AgentError::InternalError(anyhow::anyhow!("failed to set WAL mode: {e}")))?;

        let store = Self {
            db: Arc::new(Mutex::new(conn)),
            config: cfg,
        };

        // Initialize table schema
        store.init_schema().await?;

        Ok(store)
    }

    async fn init_schema(&self) -> AgentResult<()> {
        self.write(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS projects (
                    id          TEXT PRIMARY KEY,
                    name        TEXT NOT NULL,
                    cwd         TEXT NOT NULL,
                    created_at  INTEGER NOT NULL,
                    updated_at  INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS sessions (
                    id          TEXT PRIMARY KEY,
                    created_at  INTEGER NOT NULL,
                    updated_at  INTEGER NOT NULL,
                    status      TEXT NOT NULL DEFAULT 'active',
                    model       TEXT NOT NULL,
                    task        TEXT NOT NULL,
                    agent_name  TEXT,
                    project_id  TEXT REFERENCES projects(id) ON DELETE SET NULL,
                    cwd         TEXT,
                    turns       INTEGER NOT NULL DEFAULT 0,
                    tokens_used INTEGER NOT NULL DEFAULT 0,
                    parent_id   TEXT,
                    fork_point  INTEGER
                );

                CREATE TABLE IF NOT EXISTS messages (
                    id           INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id   TEXT NOT NULL REFERENCES sessions(id),
                    turn         INTEGER NOT NULL,
                    role         TEXT NOT NULL,
                    content_json TEXT NOT NULL,
                    created_at   INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS tool_calls (
                    id          TEXT PRIMARY KEY,
                    session_id  TEXT NOT NULL REFERENCES sessions(id),
                    turn        INTEGER NOT NULL,
                    tool_name   TEXT NOT NULL,
                    input_json  TEXT NOT NULL,
                    output_json TEXT,
                    error       TEXT,
                    duration_ms INTEGER,
                    created_at  INTEGER NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_messages_session
                    ON messages(session_id, turn);
                CREATE INDEX IF NOT EXISTS idx_tool_calls_session
                    ON tool_calls(session_id, turn);

                CREATE TABLE IF NOT EXISTS skill_usage (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id  TEXT,
                    project_id  TEXT,
                    skill       TEXT NOT NULL,
                    version     TEXT,
                    mode        TEXT NOT NULL,
                    ts          INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_skill_usage_skill
                    ON skill_usage(skill);
                CREATE INDEX IF NOT EXISTS idx_skill_usage_ts
                    ON skill_usage(ts);

                CREATE TABLE IF NOT EXISTS worktrees (
                    id            TEXT PRIMARY KEY,
                    session_id    TEXT REFERENCES sessions(id) ON DELETE SET NULL,
                    repo_path     TEXT NOT NULL,
                    worktree_path TEXT NOT NULL,
                    branch        TEXT,
                    base_branch   TEXT,
                    status        TEXT NOT NULL DEFAULT 'active',
                    created_at    INTEGER NOT NULL,
                    removed_at    INTEGER
                );",
            )?;
            // Migrations: add columns to existing databases (ignore errors if already present)
            let _ = conn.execute("ALTER TABLE sessions ADD COLUMN agent_name TEXT", []);
            let _ = conn.execute("ALTER TABLE sessions ADD COLUMN project_id TEXT REFERENCES projects(id) ON DELETE SET NULL", []);
            let _ = conn.execute("ALTER TABLE sessions ADD COLUMN parent_id TEXT", []);
            let _ = conn.execute("ALTER TABLE sessions ADD COLUMN fork_point INTEGER", []);
            // Named session: custom title override (NULL = host derives from task).
            let _ = conn.execute("ALTER TABLE sessions ADD COLUMN title TEXT", []);
            // Soft-archive timestamp (NULL = active).
            let _ = conn.execute("ALTER TABLE sessions ADD COLUMN archived_at INTEGER", []);
            Ok(())
        })
        .await
    }

    /// P0-1: All write operations serial entry — global Mutex lock
    /// Phase 1: direct execution without retry (database is locked is rare in WAL mode)
    /// Phase 2: add exponential backoff retry
    pub async fn write<T, F>(&self, op: F) -> AgentResult<T>
    where
        F: FnOnce(&rusqlite::Connection) -> AgentResult<T> + Send,
    {
        let db = self.db.lock().await;
        op(&db)
    }

    /// P0-1: Batch message save — avoids per-row frequent commits
    pub async fn save_messages_batch(
        &self,
        session_id: &str,
        messages: &[(usize, &Message)],
    ) -> AgentResult<()> {
        let sid = session_id.to_string();
        let msgs: Vec<(usize, String, String, i64)> = messages
            .iter()
            .map(|(turn, msg)| {
                (
                    *turn,
                    format!("{:?}", msg.role).to_lowercase(),
                    serde_json::to_string(&msg.content).unwrap_or_default(),
                    chrono::Utc::now().timestamp(),
                )
            })
            .collect();

        self.write(move |conn| {
            let tx = conn.unchecked_transaction()?;
            for (turn, role, content_json, created_at) in &msgs {
                tx.execute(
                    "INSERT INTO messages (session_id, turn, role, content_json, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![sid, turn, role, content_json, created_at],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Create session
    pub async fn create_session(
        &self,
        id: &str,
        model: &str,
        task: &str,
        agent_name: &str,
    ) -> AgentResult<()> {
        self.create_session_with_project(id, model, task, agent_name, None)
            .await
    }

    /// Create session with optional project_id
    pub async fn create_session_with_project(
        &self,
        id: &str,
        model: &str,
        task: &str,
        agent_name: &str,
        project_id: Option<&str>,
    ) -> AgentResult<()> {
        let sid = id.to_string();
        let m = model.to_string();
        let t = task.to_string();
        let an = agent_name.to_string();
        let pid = project_id.map(|s| s.to_string());
        let now = chrono::Utc::now().timestamp();

        self.write(move |conn| {
            conn.execute(
                "INSERT INTO sessions (id, created_at, updated_at, status, model, task, agent_name, project_id, turns)
                 VALUES (?1, ?2, ?3, 'active', ?4, ?5, ?6, ?7, 0)",
                rusqlite::params![sid, now, now, m, t, an, pid],
            )?;
            Ok(())
        })
        .await
    }

    /// Fork a new thread from a source session: in one transaction create a new session row
    /// (recording parent_id/fork_point), and copy the source session's message history where
    /// `messages.id < before_msg_id` (including ToolUse/ToolResult conversation context,
 /// equivalent to fork copying rollout items). Truncation semantics = before
    /// (exclusive of the boundary message); the source session stays append-only. The separate
    /// tool_calls audit table is not copied — non-model context.
    #[allow(clippy::too_many_arguments)] // signature fixed by the caller (host fork passthrough), see design doc
    pub async fn fork_session(
        &self,
        source_id: &str,
        new_id: &str,
        before_msg_id: i64,
        model: &str,
        task: &str,
        agent_name: &str,
        project_id: Option<&str>,
    ) -> AgentResult<()> {
        let src = source_id.to_string();
        let nid = new_id.to_string();
        let m = model.to_string();
        let t = task.to_string();
        let an = agent_name.to_string();
        let pid = project_id.map(|s| s.to_string());
        let now = chrono::Utc::now().timestamp();

        self.write(move |conn| {
            // Source session must exist
            let exists: bool = conn.query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                rusqlite::params![src],
                |_| Ok(true),
            ).optional()?.unwrap_or(false);
            if !exists {
                return Err(AgentError::InternalError(anyhow::anyhow!(
                    "fork_session: source session {src} not found"
                )));
            }
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO sessions (id, created_at, updated_at, status, model, task, agent_name, project_id, turns, parent_id, fork_point)
                 VALUES (?1, ?2, ?3, 'active', ?4, ?5, ?6, ?7, 0, ?8, ?9)",
                rusqlite::params![nid, now, now, m, t, an, pid, src, before_msg_id],
            )?;
            tx.execute(
                "INSERT INTO messages (session_id, turn, role, content_json, created_at)
                 SELECT ?1, turn, role, content_json, created_at FROM messages
                 WHERE session_id = ?2 AND id < ?3",
                rusqlite::params![nid, src, before_msg_id],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Update session status
    pub async fn update_session_status(
        &self,
        id: &str,
        status: &str,
        turns: usize,
        tokens_used: usize,
    ) -> AgentResult<()> {
        let sid = id.to_string();
        let st = status.to_string();
        let now = chrono::Utc::now().timestamp();
        let t = turns;
        let tok = tokens_used;

        self.write(move |conn| {
            conn.execute(
                "UPDATE sessions SET status=?1, turns=?2, tokens_used=?3, updated_at=?4 WHERE id=?5",
                rusqlite::params![st, t, tok, now, sid],
            )?;
            Ok(())
        })
        .await
    }

    /// Mark task as failed (called on cancellation)
    pub async fn mark_failed(&self, id: &str, reason: &str) -> AgentResult<()> {
        let sid = id.to_string();
        let st = format!("failed: {reason}");
        let now = chrono::Utc::now().timestamp();
        // only update status and timestamp, preserve recorded turns/tokens_used
        self.write(move |conn| {
            conn.execute(
                "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![st, now, sid],
            )?;
            Ok(())
        })
        .await
    }

    /// Save tool call record
    pub async fn save_tool_call(
        &self,
        id: &str,
        session_id: &str,
        turn: usize,
        tool_name: &str,
        input: &serde_json::Value,
        output: Option<&str>,
        error: Option<&str>,
        duration_ms: u64,
    ) -> AgentResult<()> {
        let tid = id.to_string();
        let sid = session_id.to_string();
        let name = tool_name.to_string();
        let input_json = serde_json::to_string(input).unwrap_or_default();
        let output_json = output.map(|o| o.to_string());
        let err = error.map(|e| e.to_string());
        let now = chrono::Utc::now().timestamp();

        self.write(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO tool_calls (id, session_id, turn, tool_name, input_json, output_json, error, duration_ms, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![tid, sid, turn, name, input_json, output_json, err, duration_ms, now],
            )?;
            Ok(())
        })
        .await
    }

    /// Record a skill usage event (invocation/activation). Never fails the caller.
    pub async fn record_skill_usage(
        &self,
        session_id: Option<&str>,
        project_id: Option<&str>,
        skill: &str,
        version: Option<&str>,
        mode: &str,
    ) -> AgentResult<()> {
        let now = chrono::Utc::now().timestamp();
        let sid = session_id.map(|s| s.to_string());
        let pid = project_id.map(|s| s.to_string());
        let sk = skill.to_string();
        let ver = version.map(|s| s.to_string());
        let m = mode.to_string();
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO skill_usage (session_id, project_id, skill, version, mode, ts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![sid, pid, sk, ver, m, now],
            )?;
            Ok(())
        })
        .await
    }

    /// List recent skill usage records, newest first.
    pub async fn list_skill_usage(&self, limit: usize) -> AgentResult<Vec<SkillUsageRecord>> {
        let db = self.db.lock().await;
        let mut stmt = db.prepare(
            "SELECT session_id, project_id, skill, version, mode, ts
             FROM skill_usage ORDER BY ts DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], |row| {
            Ok(SkillUsageRecord {
                session_id: row.get(0)?,
                project_id: row.get(1)?,
                skill: row.get(2)?,
                version: row.get(3)?,
                mode: row.get(4)?,
                ts: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Manually compact session messages (keep system prompt + most recent N, anchored).
    /// Returns (before_tokens, after_tokens).
    ///
    /// Uses the role-weighted estimator (counts ToolResult content, unlike the old
    /// Text-only bytes/4) and the anchor-aware compaction so the initial user goal,
    /// recent user turns and unclosed tool-call pairs survive a manual compact.
    pub async fn compact_session_messages(
        &self,
        session_id: &str,
        keep_recent: usize,
    ) -> AgentResult<(usize, usize)> {
        let sid = session_id.to_string();
        // Read ALL messages for this session (oldest first). A windowed
        // read (the old `10000` cap) combined with the unconditional DELETE
        // below silently dropped everything older than the window — exactly
        // the initial-goal messages the anchor logic exists to preserve.
        let all = self.get_recent_messages(&sid, i64::MAX as usize).await?;
        let before_tokens = crate::compaction::estimate_messages_tokens(&all);
        let anchors = crate::compaction::collect_anchors(&all, 2);
        let topo = crate::compaction::pair_tool_calls(&all);
        let compacted =
            crate::compaction::compact_messages_anchored(&all, &anchors, keep_recent, &topo.closed);
        let after_tokens = crate::compaction::estimate_messages_tokens(&compacted);
        // Replace: delete all old messages, write trimmed ones back
        let s = sid.clone();
        self.write(move |conn| {
            // One transaction: a mid-rewrite failure must not leave the
            // session with an EMPTY message table.
            let tx = conn.unchecked_transaction()?;
            tx.execute("DELETE FROM messages WHERE session_id = ?1", rusqlite::params![s])?;
            let now = chrono::Utc::now().timestamp();
            for (turn, msg) in compacted.iter().enumerate() {
                let role = format!("{:?}", msg.role).to_lowercase();
                let content_json = serde_json::to_string(&msg.content).unwrap_or_default();
                tx.execute(
                    "INSERT INTO messages (session_id, turn, role, content_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![s, turn, role, content_json, now],
                )?;
            }
            tx.commit()?;
            Ok(())
        }).await?;
        Ok((before_tokens, after_tokens))
    }

    /// P0-1: Periodic WAL checkpoint — prevents unbounded wal file growth
    pub async fn periodic_checkpoint(&self) -> AgentResult<()> {
        self.write(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            Ok(())
        })
        .await
    }

    /// Clean up completed sessions past retention period
    pub async fn cleanup_old_sessions(&self) -> AgentResult<usize> {
        let retention_days = self.config.session_retention_days;
        let cutoff = chrono::Utc::now().timestamp() - (retention_days as i64 * 86400);

        let result = self
            .write(move |conn| {
                let tx = conn.unchecked_transaction()?;
                let m = tx.execute(
                    "DELETE FROM messages WHERE session_id IN
                        (SELECT id FROM sessions WHERE status = 'completed' AND updated_at < ?1)",
                    rusqlite::params![cutoff],
                )?;
                let tc = tx.execute(
                    "DELETE FROM tool_calls WHERE session_id IN
                        (SELECT id FROM sessions WHERE status = 'completed' AND updated_at < ?1)",
                    rusqlite::params![cutoff],
                )?;
                let s = tx.execute(
                    "DELETE FROM sessions WHERE status = 'completed' AND updated_at < ?1",
                    rusqlite::params![cutoff],
                )?;
                tx.commit()?;
                Ok(m + tc + s)
            })
            .await?;

        if result > 0 {
            tracing::info!(deleted = result, "cleanup of expired sessions complete");
        }
        Ok(result)
    }

    /// Get recent messages of a session (for recovery)
    pub async fn get_recent_messages(
        &self,
        session_id: &str,
        limit: usize,
    ) -> AgentResult<Vec<Message>> {
        let sid = session_id.to_string();
        let db = self.db.lock().await;

        let mut stmt = db.prepare(
            "SELECT role, content_json FROM messages
             WHERE session_id = ?1 AND role IN ('user','assistant','system','tool')
             ORDER BY id DESC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(rusqlite::params![sid, limit as i64], |row| {
            let role: String = row.get(0)?;
            let content_json: String = row.get(1)?;
            Ok((role, content_json))
        })?;

        let mut messages = Vec::new();
        for row in rows {
            let (role, content_json) = row?;
            let role = match role.as_str() {
                "user" => starriver_kernel::types::Role::User,
                "assistant" => starriver_kernel::types::Role::Assistant,
                "system" => starriver_kernel::types::Role::System,
                "tool" => starriver_kernel::types::Role::Tool,
                _ => continue,
            };
            let content = serde_json::from_str(&content_json).unwrap_or_default();
            messages.push(Message {
                meta: None,
                role,
                content,
            });
        }

        // retrieved most recent limit rows (DESC), restored to chronological order
        messages.reverse();
        Ok(messages)
    }

    /// Load the most recent `limit` messages of a session in chronological order for the
    /// model request, excluding stored system prompts.
    ///
    /// System is rebuilt per-execute from the agent config, so persisting it per turn was
    /// pure redundancy (and re-loading it would inject stale agent prompts). Unlike
    /// `get_recent_messages`, ordering is by insertion id (not per-execute `turn`, which
    /// restarts at 0 on every execute and would interleave multi-turn runs incorrectly).
    pub async fn get_session_history(
        &self,
        session_id: &str,
        limit: usize,
    ) -> AgentResult<Vec<Message>> {
        let sid = session_id.to_string();
        let db = self.db.lock().await;

        let mut stmt = db.prepare(
            "SELECT role, content_json FROM messages
             WHERE session_id = ?1 AND role != 'system'
             ORDER BY id DESC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(rusqlite::params![sid, limit as i64], |row| {
            let role: String = row.get(0)?;
            let content_json: String = row.get(1)?;
            Ok((role, content_json))
        })?;

        let mut messages = Vec::new();
        for row in rows {
            let (role, content_json) = row?;
            let role = match role.as_str() {
                "user" => starriver_kernel::types::Role::User,
                "assistant" => starriver_kernel::types::Role::Assistant,
                "tool" => starriver_kernel::types::Role::Tool,
                _ => continue,
            };
            let content = serde_json::from_str(&content_json).unwrap_or_default();
            messages.push(Message {
                meta: None,
                role,
                content,
            });
        }

        // most recent first → restored to chronological order
        messages.reverse();
        Ok(messages)
    }

    /// Chronological messages of a session with their DB ids (`messages.id`),
    /// used by the backtrack timeline to locate the fork cutoff.
    pub async fn get_session_history_with_ids(
        &self,
        session_id: &str,
        limit: usize,
    ) -> AgentResult<Vec<(i64, Message)>> {
        let sid = session_id.to_string();
        let db = self.db.lock().await;

        let mut stmt = db.prepare(
            "SELECT id, role, content_json FROM messages
             WHERE session_id = ?1 AND role != 'system'
             ORDER BY id DESC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(rusqlite::params![sid, limit], |row| {
            let id: i64 = row.get(0)?;
            let role: String = row.get(1)?;
            let content_json: String = row.get(2)?;
            Ok((id, role, content_json))
        })?;

        let mut messages = Vec::new();
        for row in rows {
            let (id, role, content_json) = row?;
            let role = match role.as_str() {
                "user" => starriver_kernel::types::Role::User,
                "assistant" => starriver_kernel::types::Role::Assistant,
                "tool" => starriver_kernel::types::Role::Tool,
                _ => continue,
            };
            let content = serde_json::from_str(&content_json).unwrap_or_default();
            messages.push((
                id,
                Message {
                    meta: None,
                    role,
                    content,
                },
            ));
        }

        messages.reverse();
        Ok(messages)
    }

    /// List sessions with optional project_id filter.
    /// Pass None to return all sessions (backward compatible with Chat Tab).
    pub async fn list_sessions(&self) -> AgentResult<Vec<SessionSummary>> {
        self.list_sessions_filtered(None).await
    }

    /// List sessions filtered by optional project_id.
    pub async fn list_sessions_filtered(
        &self,
        project_id: Option<&str>,
    ) -> AgentResult<Vec<SessionSummary>> {
        let db = self.db.lock().await;
        if let Some(pid) = project_id {
            let mut stmt = db.prepare(
                "SELECT id, model, task, agent_name, project_id, created_at, turns, status, parent_id, fork_point, title, archived_at
                 FROM sessions WHERE project_id = ?1 AND archived_at IS NULL ORDER BY created_at DESC, id DESC LIMIT 10000",
            )?;
            let rows = stmt.query_map(rusqlite::params![pid], |row| {
                Ok(SessionSummary {
                    id: row.get(0)?,
                    model: row.get(1)?,
                    task: row.get(2)?,
                    agent_name: row.get(3)?,
                    project_id: row.get(4)?,
                    created_at: row.get(5)?,
                    turns: row.get(6)?,
                    status: row.get(7)?,
                    parent_id: row.get(8)?,
                    fork_point: row.get(9)?,
                    title: row.get(10)?,
                    archived_at: row.get(11)?,
                })
            })?;
            let mut sessions = Vec::new();
            for row in rows {
                sessions.push(row?);
            }
            Ok(sessions)
        } else {
            let mut stmt = db.prepare(
                "SELECT id, model, task, agent_name, project_id, created_at, turns, status, parent_id, fork_point, title, archived_at
                 FROM sessions WHERE archived_at IS NULL ORDER BY created_at DESC, id DESC LIMIT 10000",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(SessionSummary {
                    id: row.get(0)?,
                    model: row.get(1)?,
                    task: row.get(2)?,
                    agent_name: row.get(3)?,
                    project_id: row.get(4)?,
                    created_at: row.get(5)?,
                    turns: row.get(6)?,
                    status: row.get(7)?,
                    parent_id: row.get(8)?,
                    fork_point: row.get(9)?,
                    title: row.get(10)?,
                    archived_at: row.get(11)?,
                })
            })?;
            let mut sessions = Vec::new();
            for row in rows {
                sessions.push(row?);
            }
            Ok(sessions)
        }
    }

    /// Fetch a single session's summary by id (None when missing).
    pub async fn get_session(&self, session_id: &str) -> AgentResult<Option<SessionSummary>> {
        let sid = session_id.to_string();
        let db = self.db.lock().await;
        let mut stmt = db.prepare(
            "SELECT id, model, task, agent_name, project_id, created_at, turns, status, parent_id, fork_point, title, archived_at
             FROM sessions WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![sid], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                model: row.get(1)?,
                task: row.get(2)?,
                agent_name: row.get(3)?,
                project_id: row.get(4)?,
                created_at: row.get(5)?,
                turns: row.get(6)?,
                status: row.get(7)?,
                parent_id: row.get(8)?,
                fork_point: row.get(9)?,
                title: row.get(10)?,
                archived_at: row.get(11)?,
            })
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    // ── Project CRUD ──

    /// Create a new project
    pub async fn create_project(&self, name: &str, cwd: &str) -> AgentResult<ProjectSummary> {
        let id = uuid::Uuid::new_v4().to_string();
        let id_for_return = id.clone();
        let n = name.to_string();
        let c = cwd.to_string();
        let now = chrono::Utc::now().timestamp();

        self.write(move |conn| {
            conn.execute(
                "INSERT INTO projects (id, name, cwd, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![id, n, c, now, now],
            )?;
            Ok(())
        })
        .await?;

        Ok(ProjectSummary {
            id: id_for_return,
            name: name.to_string(),
            cwd: cwd.to_string(),
            created_at: now,
            session_count: 0,
        })
    }

    /// List all projects with session counts
    pub async fn list_projects(&self) -> AgentResult<Vec<ProjectSummary>> {
        let db = self.db.lock().await;
        let mut stmt = db.prepare(
            "SELECT p.id, p.name, p.cwd, p.created_at,
                    (SELECT COUNT(*) FROM sessions s WHERE s.project_id = p.id) as session_count
             FROM projects p ORDER BY p.updated_at DESC LIMIT 10000",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ProjectSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                cwd: row.get(2)?,
                created_at: row.get(3)?,
                session_count: row.get::<_, i64>(4)? as u32,
            })
        })?;
        let mut projects = Vec::new();
        for row in rows {
            projects.push(row?);
        }
        Ok(projects)
    }

    /// Delete a project and all its sessions
    pub async fn delete_project(&self, project_id: &str) -> AgentResult<usize> {
        let pid = project_id.to_string();
        self.write(move |conn| {
            let tx = conn.unchecked_transaction()?;
            // BUG-34: child rows first — FK enforcement is on, so deleting
            // sessions while messages/tool_calls still reference them failed
            // the whole call (delete_session/cleanup_old_sessions already
            // follow this order).
            tx.execute(
                "DELETE FROM messages WHERE session_id IN
                    (SELECT id FROM sessions WHERE project_id = ?1)",
                rusqlite::params![pid],
            )?;
            tx.execute(
                "DELETE FROM tool_calls WHERE session_id IN
                    (SELECT id FROM sessions WHERE project_id = ?1)",
                rusqlite::params![pid],
            )?;
            let s = tx.execute(
                "DELETE FROM sessions WHERE project_id = ?1",
                rusqlite::params![pid],
            )?;
            let p = tx.execute("DELETE FROM projects WHERE id = ?1", rusqlite::params![pid])?;
            tx.commit()?;
            Ok(s + p)
        })
        .await
    }

    /// Update project name and/or cwd
    pub async fn update_project(
        &self,
        project_id: &str,
        name: Option<&str>,
        cwd: Option<&str>,
    ) -> AgentResult<()> {
        let pid = project_id.to_string();
        let now = chrono::Utc::now().timestamp();
        let n = name.map(|s| s.to_string());
        let c = cwd.map(|s| s.to_string());

        self.write(move |conn| {
            if let Some(ref name_val) = n {
                conn.execute(
                    "UPDATE projects SET name = ?1, updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![name_val, now, pid],
                )?;
            }
            if let Some(ref cwd_val) = c {
                conn.execute(
                    "UPDATE projects SET cwd = ?1, updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![cwd_val, now, pid],
                )?;
            }
            Ok(())
        })
        .await
    }

    /// Get a single project by id
    pub async fn get_project(&self, project_id: &str) -> AgentResult<Option<ProjectSummary>> {
        let db = self.db.lock().await;
        let pid = project_id.to_string();
        let mut stmt = db.prepare(
            "SELECT p.id, p.name, p.cwd, p.created_at,
                    (SELECT COUNT(*) FROM sessions s WHERE s.project_id = p.id) as session_count
             FROM projects p WHERE p.id = ?1",
        )?;
        let result = stmt.query_row(rusqlite::params![pid], |row| {
            Ok(ProjectSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                cwd: row.get(2)?,
                created_at: row.get(3)?,
                session_count: row.get::<_, i64>(4)? as u32,
            })
        });
        match result {
            Ok(p) => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AgentError::InternalError(anyhow::anyhow!("{e}"))),
        }
    }

    // ── Worktree CRUD ──

    /// Insert a worktree record (idempotent by id).
    pub async fn create_worktree(&self, row: &WorktreeRow) -> AgentResult<()> {
        let id = row.id.clone();
        let sid = row.session_id.clone();
        let repo = row.repo_path.clone();
        let wt = row.worktree_path.clone();
        let branch = row.branch.clone();
        let base = row.base_branch.clone();
        let status = row.status.clone();
        let created = row.created_at;
        let removed = row.removed_at;
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO worktrees (id, session_id, repo_path, worktree_path, branch, base_branch, status, created_at, removed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET session_id=excluded.session_id, repo_path=excluded.repo_path,
                   worktree_path=excluded.worktree_path, branch=excluded.branch, base_branch=excluded.base_branch,
                   status=excluded.status, created_at=excluded.created_at, removed_at=excluded.removed_at",
                rusqlite::params![id, sid, repo, wt, branch, base, status, created, removed],
            )?;
            Ok(())
        }).await
    }

    /// Latest worktree record for a session.
    pub async fn get_worktree_by_session(
        &self,
        session_id: &str,
    ) -> AgentResult<Option<WorktreeRow>> {
        let db = self.db.lock().await;
        let sid = session_id.to_string();
        let mut stmt = db.prepare(
            "SELECT id, session_id, repo_path, worktree_path, branch, base_branch, status, created_at, removed_at
             FROM worktrees WHERE session_id = ?1 ORDER BY created_at DESC, id DESC LIMIT 1",
        )?;
        let result = stmt.query_row(rusqlite::params![sid], |row| {
            Ok(WorktreeRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                repo_path: row.get(2)?,
                worktree_path: row.get(3)?,
                branch: row.get(4)?,
                base_branch: row.get(5)?,
                status: row.get(6)?,
                created_at: row.get(7)?,
                removed_at: row.get(8)?,
            })
        });
        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AgentError::InternalError(anyhow::anyhow!("{e}"))),
        }
    }

    /// Look up a record by worktree path.
    pub async fn get_worktree_by_path(
        &self,
        worktree_path: &str,
    ) -> AgentResult<Option<WorktreeRow>> {
        let db = self.db.lock().await;
        let wt = worktree_path.to_string();
        let mut stmt = db.prepare(
            "SELECT id, session_id, repo_path, worktree_path, branch, base_branch, status, created_at, removed_at
             FROM worktrees WHERE worktree_path = ?1 LIMIT 1",
        )?;
        let result = stmt.query_row(rusqlite::params![wt], |row| {
            Ok(WorktreeRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                repo_path: row.get(2)?,
                worktree_path: row.get(3)?,
                branch: row.get(4)?,
                base_branch: row.get(5)?,
                status: row.get(6)?,
                created_at: row.get(7)?,
                removed_at: row.get(8)?,
            })
        });
        match result {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(AgentError::InternalError(anyhow::anyhow!("{e}"))),
        }
    }

    /// Update worktree status and deletion time.
    pub async fn update_worktree_status(
        &self,
        id: &str,
        status: &str,
        removed_at: Option<i64>,
    ) -> AgentResult<()> {
        let id = id.to_string();
        let st = status.to_string();
        let ra = removed_at;
        self.write(move |conn| {
            conn.execute(
                "UPDATE worktrees SET status = ?1, removed_at = ?2 WHERE id = ?3",
                rusqlite::params![st, ra, id],
            )?;
            Ok(())
        })
        .await
    }

    /// All retained worktrees (the sidebar "kept" section).
    pub async fn list_kept_worktrees(&self) -> AgentResult<Vec<WorktreeRow>> {
        let db = self.db.lock().await;
        let mut stmt = db.prepare(
            "SELECT id, session_id, repo_path, worktree_path, branch, base_branch, status, created_at, removed_at
             FROM worktrees WHERE status = 'kept' ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(WorktreeRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                repo_path: row.get(2)?,
                worktree_path: row.get(3)?,
                branch: row.get(4)?,
                base_branch: row.get(5)?,
                status: row.get(6)?,
                created_at: row.get(7)?,
                removed_at: row.get(8)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Delete a session and its messages
    pub async fn delete_session(&self, session_id: &str) -> AgentResult<()> {
        let sid = session_id.to_string();
        self.write(move |conn| {
            let tx = conn.unchecked_transaction()?;
            // Promote the child branch to a root session (no cascade delete) to avoid a dangling parent_id.
            tx.execute(
                "UPDATE sessions SET parent_id = NULL WHERE parent_id = ?1",
                rusqlite::params![sid],
            )?;
            tx.execute(
                "DELETE FROM tool_calls WHERE session_id = ?1",
                rusqlite::params![sid],
            )?;
            tx.execute(
                "DELETE FROM messages WHERE session_id = ?1",
                rusqlite::params![sid],
            )?;
            tx.execute("DELETE FROM sessions WHERE id = ?1", rusqlite::params![sid])?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Name a session: `Some(title)` sets a custom title override, `None` clears the override
    /// back to the task-derived title. Does not touch the `task` column (continuation context stays intact).
    pub async fn rename_session(&self, session_id: &str, title: Option<&str>) -> AgentResult<()> {
        let sid = session_id.to_string();
        let t = title.map(|s| s.to_string());
        self.write(move |conn| {
            conn.execute(
                "UPDATE sessions SET title = NULLIF(?2, '') WHERE id = ?1",
                rusqlite::params![sid, t],
            )?;
            Ok(())
        })
        .await
    }

    /// Soft archive/restore: `archived=true` writes the archive timestamp, `false` clears it
    /// (back to active). Archiving does not physically delete; `get_session`/continuation still
    /// reach it, while the default list (`list_sessions_filtered`) excludes it.
    pub async fn set_session_archived(&self, session_id: &str, archived: bool) -> AgentResult<()> {
        let sid = session_id.to_string();
        let now = chrono::Utc::now().timestamp();
        self.write(move |conn| {
            conn.execute(
                "UPDATE sessions SET archived_at = CASE WHEN ?2 THEN ?3 ELSE NULL END WHERE id = ?1",
                rusqlite::params![sid, archived, now],
            )?;
            Ok(())
        })
        .await
    }

    /// Archived sessions only (data source for the `/unarchive` picker), newest archive first.
    pub async fn list_archived_sessions(&self) -> AgentResult<Vec<SessionSummary>> {
        let db = self.db.lock().await;
        let mut stmt = db.prepare(
            "SELECT id, model, task, agent_name, project_id, created_at, turns, status, parent_id, fork_point, title, archived_at
             FROM sessions WHERE archived_at IS NOT NULL ORDER BY archived_at DESC, id DESC LIMIT 10000",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                model: row.get(1)?,
                task: row.get(2)?,
                agent_name: row.get(3)?,
                project_id: row.get(4)?,
                created_at: row.get(5)?,
                turns: row.get(6)?,
                status: row.get(7)?,
                parent_id: row.get(8)?,
                fork_point: row.get(9)?,
                title: row.get(10)?,
                archived_at: row.get(11)?,
            })
        })?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        Ok(sessions)
    }
}

/// Worktree record — index only; live state comes from `git worktree list`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorktreeRow {
    pub id: String,
    pub session_id: Option<String>,
    pub repo_path: String,
    pub worktree_path: String,
    pub branch: Option<String>,
    pub base_branch: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub removed_at: Option<i64>,
}

/// Summary for project list display
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub created_at: i64,
    pub session_count: u32,
}

/// Summary for session list display
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub project_id: Option<String>,
    pub id: String,
    pub model: String,
    pub task: String,
    pub agent_name: Option<String>,
    pub created_at: i64,
    pub turns: usize,
    pub status: String,
    /// Fork parent session id (NULL = root session)
    pub parent_id: Option<String>,
    /// Fork truncation point: source session messages.id boundary (the new thread copies messages with id < fork_point)
    pub fork_point: Option<i64>,
    /// Custom title override (None = host derives from task)
    pub title: Option<String>,
    /// Soft-archive timestamp (None = active)
    pub archived_at: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::config::SessionConfig;
    use starriver_kernel::types::{ContentBlock, Message, Role};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_db(name: &str) -> SessionConfig {
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "starriver-sess-{}-{}-{}.db",
            name,
            std::process::id(),
            n
        ));
        SessionConfig {
            db_path: p.to_string_lossy().to_string(),
            max_sessions: 100,
            checkpoint_interval_secs: 30,
            wal_checkpoint_interval_secs: 300,
            write_retry_max: 3,
            write_retry_base_ms: 10,
            session_retention_days: 30,
        }
    }

    fn cleanup(db_path: &str) {
        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_file(format!("{db_path}-wal"));
        let _ = std::fs::remove_file(format!("{db_path}-shm"));
    }

    #[test]
    fn resolve_db_path_anchors_relative_to_starriver_home() {
        let home = std::env::var("HOME").unwrap();
        // default relative path → ~/.starriver/starriver.db
        assert_eq!(
            resolve_session_db_path("./starriver.db"),
            PathBuf::from(&home)
                .join(".starriver")
                .join("./starriver.db")
        );
        // ~/ expansion
        assert_eq!(
            resolve_session_db_path("~/x.db"),
            PathBuf::from(&home).join("x.db")
        );
        // absolute passes through untouched (a POSIX path is not absolute on
        // Windows — `is_absolute` requires a drive prefix there)
        #[cfg(unix)]
        assert_eq!(
            resolve_session_db_path("/tmp/abs.db"),
            PathBuf::from("/tmp/abs.db")
        );
        #[cfg(windows)]
        assert_eq!(
            resolve_session_db_path(r"C:\tmp\abs.db"),
            PathBuf::from(r"C:\tmp\abs.db")
        );
    }

    #[tokio::test]
    async fn open_initializes_schema() {
        let cfg = temp_db("schema");
        let store = SessionStore::open(&cfg).await.unwrap();
        // directly query sqlite_master to verify tables exist
        let path = cfg.db_path.clone();
        let count = store.write(|conn| {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('projects','sessions','messages','tool_calls')",
                [], |r| r.get(0),
            )?;
            Ok(n)
        }).await.unwrap();
        assert_eq!(count, 4);
        cleanup(&path);
    }

    #[tokio::test]
    async fn skill_usage_record_and_list() {
        let cfg = temp_db("skill-usage");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .record_skill_usage(
                Some("s1"),
                Some("p1"),
                "write-book",
                Some("0.1.0"),
                "command",
            )
            .await
            .unwrap();
        store
            .record_skill_usage(None, None, "review", None, "toggle")
            .await
            .unwrap();

        let all = store.list_skill_usage(10).await.unwrap();
        assert_eq!(all.len(), 2, "both records listed");
        let first = &all[0]; // newest first
        assert_eq!(first.skill, "review");
        assert_eq!(first.mode, "toggle");
        assert!(first.session_id.is_none());
        let second = &all[1];
        assert_eq!(second.skill, "write-book");
        assert_eq!(second.mode, "command");
        assert_eq!(second.session_id.as_deref(), Some("s1"));
        assert_eq!(second.project_id.as_deref(), Some("p1"));

        // Limit applies
        let one = store.list_skill_usage(1).await.unwrap();
        assert_eq!(one.len(), 1);
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn create_and_get_recent_empty() {
        let cfg = temp_db("empty");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s1", "qwen3:4b", "test task", "test-agent")
            .await
            .unwrap();
        let msgs = store.get_recent_messages("s1", 10).await.unwrap();
        assert!(msgs.is_empty());
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn save_messages_batch_and_retrieve_in_order() {
        let cfg = temp_db("batch");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s2", "m", "t", "test-agent")
            .await
            .unwrap();
        let u1 = Message::user("hello");
        let a1 = Message::assistant("hi");
        let u2 = Message::user("again");
        let batch: Vec<(usize, &Message)> = vec![(0, &u1), (1, &a1), (2, &u2)];
        store.save_messages_batch("s2", &batch).await.unwrap();
        let msgs = store.get_recent_messages("s2", 10).await.unwrap();
        assert_eq!(msgs.len(), 3);
        // order should be chronological: u1 → a1 → u2
        assert!(matches!(msgs[0].role, starriver_kernel::types::Role::User));
        assert!(matches!(
            msgs[1].role,
            starriver_kernel::types::Role::Assistant
        ));
        assert!(matches!(msgs[2].role, starriver_kernel::types::Role::User));
        cleanup(&cfg.db_path);
    }

    /// BUG-34 regression: `delete_project` claims "Delete a project and all
    /// its sessions" — with FK enforcement on, deleting sessions that have
    /// messages/tool_calls rows must clean those children first (the sibling
    /// `delete_session` does exactly that), not fail the whole call.
    #[tokio::test]
    async fn delete_project_cascades_sessions_with_messages() {
        let cfg = temp_db("delproj");
        let store = SessionStore::open(&cfg).await.unwrap();
        let project = store.create_project("p", "/tmp").await.unwrap();
        store
            .create_session_with_project("s-del", "m", "t", "agent", Some(&project.id))
            .await
            .unwrap();
        store
            .save_messages_batch(
                "s-del",
                &[(0, &Message::user("hello"))],
            )
            .await
            .unwrap();
        store
            .delete_project(&project.id)
            .await
            .expect("deleting a project must cascade its sessions' messages");
        let sessions = store.list_sessions().await.unwrap();
        assert!(
            sessions.iter().all(|s| s.id != "s-del"),
            "session gone with the project"
        );
        cleanup(&cfg.db_path);
    }

    /// BUG-35 regression: `turn` restarts at 0 on every execute — ordering by
    /// `turn DESC` interleaves runs and lets LIMIT pick the wrong rows. The
    /// sibling `get_session_history` documents exactly this ("ordering is by
    /// insertion id, not per-execute turn"); `get_recent_messages` must agree.
    #[tokio::test]
    async fn get_recent_messages_orders_by_insertion_across_runs() {
        let cfg = temp_db("recent");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s-rec", "m", "t", "test-agent")
            .await
            .unwrap();
        // An older execute run at turn 2 …
        store
            .save_messages_batch("s-rec", &[(2, &Message::assistant("done"))])
            .await
            .unwrap();
        // … then a newer execute run restarting at turn 0.
        store
            .save_messages_batch(
                "s-rec",
                &[(0, &Message::user("next")), (0, &Message::assistant("sure"))],
            )
            .await
            .unwrap();
        let msgs = store.get_recent_messages("s-rec", 10).await.unwrap();
        let texts: Vec<&str> = msgs
            .iter()
            .filter_map(|m| match m.content.first() {
                Some(starriver_kernel::types::ContentBlock::Text { text: t }) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["done", "next", "sure"],
            "chronological insertion order, not per-run turn order (got {texts:?})"
        );
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn get_session_history_excludes_system_and_orders_by_insertion() {
        let cfg = temp_db("hist");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s-hist", "m", "t", "test-agent")
            .await
            .unwrap();

        // Insertion order: a turn-2 run's final message, then a fresh turn-0 exchange
        // (turn restarts at 0 per execute — ordering must follow id, not turn).
        let a_final = Message::assistant("done");
        let b_user = Message::user("next");
        let b_assistant = Message::assistant("sure");
        let sys = Message::system("system prompt");
        store
            .save_messages_batch("s-hist", &vec![(2, &a_final)])
            .await
            .unwrap();
        store
            .save_messages_batch("s-hist", &vec![(0, &b_user), (0, &b_assistant), (0, &sys)])
            .await
            .unwrap();

        let hist = store.get_session_history("s-hist", 10).await.unwrap();
        assert!(
            hist.iter()
                .all(|m| !matches!(m.role, starriver_kernel::types::Role::System)),
            "system prompts excluded"
        );

        let texts: Vec<String> = hist
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .find_map(|c| match c {
                        starriver_kernel::types::ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            texts,
            vec!["done", "next", "sure"],
            "chronological by insertion id, not per-execute turn"
        );
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn get_session_history_respects_limit_keeping_most_recent() {
        let cfg = temp_db("hist-limit");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s-hl", "m", "t", "test-agent")
            .await
            .unwrap();
        let msgs: Vec<Message> = (0..5).map(|i| Message::user(&format!("u{i}"))).collect();
        let batch: Vec<(usize, &Message)> = msgs.iter().map(|m| (0, m)).collect();
        store.save_messages_batch("s-hl", &batch).await.unwrap();

        let hist = store.get_session_history("s-hl", 2).await.unwrap();
        let texts: Vec<String> = hist
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .find_map(|c| match c {
                        starriver_kernel::types::ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            texts,
            vec!["u3", "u4"],
            "limit keeps the most recent, in order"
        );
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn get_session_history_unknown_or_empty_session_returns_empty() {
        let cfg = temp_db("hist-empty");
        let store = SessionStore::open(&cfg).await.unwrap();

        // A session that was never created (or never wrote messages) yields an empty vec,
        // not an error — execute() relies on `.unwrap_or_default()` handling this silently.
        let hist = store
            .get_session_history("no-such-session", 10)
            .await
            .unwrap();
        assert!(hist.is_empty(), "unknown session must yield empty history");

        // A created-but-empty session is equally fine.
        store
            .create_session("s-empty", "m", "t", "test-agent")
            .await
            .unwrap();
        let hist2 = store.get_session_history("s-empty", 10).await.unwrap();
        assert!(hist2.is_empty(), "empty session must yield empty history");
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn update_session_status_persists() {
        let cfg = temp_db("status");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s3", "m", "t", "test-agent")
            .await
            .unwrap();
        store
            .update_session_status("s3", "completed", 7, 1234)
            .await
            .unwrap();
        // verify via external connection
        let conn = rusqlite::Connection::open(&cfg.db_path).unwrap();
        let (status, turns, tokens): (String, i64, i64) = conn
            .query_row(
                "SELECT status, turns, tokens_used FROM sessions WHERE id='s3'",
                [],
                |r| {
                    let s: String = r.get(0)?;
                    let t: i64 = r.get(1)?;
                    let tok: i64 = r.get(2)?;
                    Ok((s, t, tok))
                },
            )
            .unwrap();
        assert_eq!(status, "completed");
        assert_eq!(turns, 7);
        assert_eq!(tokens, 1234);
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn mark_failed_sets_status_with_reason() {
        let cfg = temp_db("fail");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s4", "m", "t", "test-agent")
            .await
            .unwrap();
        store.mark_failed("s4", "task cancelled").await.unwrap();
        let conn = rusqlite::Connection::open(&cfg.db_path).unwrap();
        let status: String = conn
            .query_row("SELECT status FROM sessions WHERE id='s4'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "failed: task cancelled");
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn save_tool_call_replaces_on_duplicate_id() {
        let cfg = temp_db("tool");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s5", "m", "t", "test-agent")
            .await
            .unwrap();
        let input = serde_json::json!({"path":"a.txt"});
        store
            .save_tool_call("tc1", "s5", 0, "read", &input, Some("ok"), None, 12)
            .await
            .unwrap();
        // save again with same id → INSERT OR REPLACE overwrites
        let input2 = serde_json::json!({"path":"b.txt"});
        store
            .save_tool_call("tc1", "s5", 0, "read", &input2, Some("ok2"), None, 34)
            .await
            .unwrap();
        let conn = rusqlite::Connection::open(&cfg.db_path).unwrap();
        let (count, out): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), output_json FROM tool_calls WHERE id='tc1'",
                [],
                |r| {
                    let c: i64 = r.get(0)?;
                    let o: String = r.get(1)?;
                    Ok((c, o))
                },
            )
            .unwrap();
        assert_eq!(count, 1, "same id should be REPLACEd, not inserted anew");
        assert!(out.contains("ok2"));
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn cleanup_old_sessions_deletes_expired_completed() {
        let cfg = temp_db("cleanup");
        let store = SessionStore::open(&cfg).await.unwrap();
        // directly insert a session completed 40 days ago
        let old_ts = chrono::Utc::now().timestamp() - 40 * 86400;
        {
            let conn = rusqlite::Connection::open(&cfg.db_path).unwrap();
            conn.execute(
                "INSERT INTO sessions (id, created_at, updated_at, status, model, task, agent_name, turns, tokens_used)
                 VALUES ('old', ?1, ?1, 'completed', 'm', 't', NULL, 0, 0)",
                rusqlite::params![old_ts],
            ).unwrap();
            conn.execute(
                "INSERT INTO sessions (id, created_at, updated_at, status, model, task, agent_name, turns, tokens_used)
                 VALUES ('active', ?1, ?1, 'active', 'm', 't', NULL, 0, 0)",
                rusqlite::params![old_ts],
            ).unwrap();
        }
        let deleted = store.cleanup_old_sessions().await.unwrap();
        assert!(deleted >= 1, "should delete expired completed sessions");
        let conn = rusqlite::Connection::open(&cfg.db_path).unwrap();
        let old_n: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions WHERE id='old'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let active_n: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions WHERE id='active'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(old_n, 0, "expired completed sessions should be deleted");
        assert_eq!(
            active_n, 1,
            "active sessions even if expired should not be cleaned up"
        );
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn periodic_checkpoint_runs_without_error() {
        let cfg = temp_db("ckpt");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s6", "m", "t", "test-agent")
            .await
            .unwrap();
        store.periodic_checkpoint().await.unwrap();
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn get_recent_messages_unknown_session_returns_empty() {
        let cfg = temp_db("unknown");
        let store = SessionStore::open(&cfg).await.unwrap();
        let msgs = store.get_recent_messages("nope", 5).await.unwrap();
        assert!(msgs.is_empty());
        cleanup(&cfg.db_path);
    }

    // ── Project tests ──

    #[tokio::test]
    async fn create_and_list_projects() {
        let cfg = temp_db("proj-list");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p1 = store
            .create_project("my-app", "/home/user/my-app")
            .await
            .unwrap();
        let _p2 = store
            .create_project("demo", "/home/user/demo")
            .await
            .unwrap();
        assert_eq!(p1.name, "my-app");
        assert_eq!(p1.session_count, 0);
        let projects = store.list_projects().await.unwrap();
        assert_eq!(projects.len(), 2);
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn worktree_crud_roundtrip() {
        let cfg = temp_db("wt-crud");
        let store = SessionStore::open(&cfg).await.unwrap();
        // worktrees.session_id foreign key references sessions(id); create sessions first
        store
            .create_session("s1", "m", "t1", "Coding Agent")
            .await
            .unwrap();
        let row = WorktreeRow {
            id: "w1".into(),
            session_id: Some("s1".into()),
            repo_path: "/r".into(),
            worktree_path: "/r/.worktrees/wt".into(),
            branch: Some("starriver/abc-fix".into()),
            base_branch: Some("main".into()),
            status: "active".into(),
            created_at: 1,
            removed_at: None,
        };
        store.create_worktree(&row).await.unwrap();
        store.create_worktree(&row).await.unwrap(); // idempotent
        let got = store.get_worktree_by_session("s1").await.unwrap().unwrap();
        assert_eq!(got.worktree_path, "/r/.worktrees/wt");
        assert_eq!(
            store
                .get_worktree_by_path("/r/.worktrees/wt")
                .await
                .unwrap()
                .unwrap()
                .id,
            "w1"
        );
        store
            .update_worktree_status("w1", "kept", None)
            .await
            .unwrap();
        assert_eq!(store.list_kept_worktrees().await.unwrap().len(), 1);
        store
            .update_worktree_status("w1", "removed", Some(9))
            .await
            .unwrap();
        assert!(store.list_kept_worktrees().await.unwrap().is_empty());
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn delete_project_cascades_sessions() {
        let cfg = temp_db("proj-del");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store.create_project("test", "/tmp/test").await.unwrap();
        // Create sessions in the project
        store
            .create_session_with_project("s1", "m", "t1", "Coding Agent", Some(&p.id))
            .await
            .unwrap();
        store
            .create_session_with_project("s2", "m", "t2", "Coding Agent", Some(&p.id))
            .await
            .unwrap();
        // Also create a session without project
        store
            .create_session("s3", "m", "t3", "Chat Agent")
            .await
            .unwrap();
        // Delete project
        let deleted = store.delete_project(&p.id).await.unwrap();
        assert!(
            deleted >= 3,
            "should delete project + 2 sessions = 3 rows, got {deleted}"
        );
        // Verify sessions in project are gone
        let sessions = store.list_sessions().await.unwrap();
        assert!(
            !sessions.iter().any(|s| s.id == "s1" || s.id == "s2"),
            "project sessions should be deleted"
        );
        assert!(
            sessions.iter().any(|s| s.id == "s3"),
            "non-project session should survive"
        );
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn update_project_name_and_cwd() {
        let cfg = temp_db("proj-upd");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store.create_project("old-name", "/old/path").await.unwrap();
        store
            .update_project(&p.id, Some("new-name"), Some("/new/path"))
            .await
            .unwrap();
        let updated = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(updated.name, "new-name");
        assert_eq!(updated.cwd, "/new/path");
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn list_sessions_filtered_by_project() {
        let cfg = temp_db("proj-filter");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p1 = store.create_project("proj-a", "/a").await.unwrap();
        let p2 = store.create_project("proj-b", "/b").await.unwrap();
        store
            .create_session_with_project("sa1", "m", "task-a1", "Coding Agent", Some(&p1.id))
            .await
            .unwrap();
        store
            .create_session_with_project("sa2", "m", "task-a2", "Coding Agent", Some(&p1.id))
            .await
            .unwrap();
        store
            .create_session_with_project("sb1", "m", "task-b1", "Coding Agent", Some(&p2.id))
            .await
            .unwrap();
        store
            .create_session("sx", "m", "no-project", "Chat Agent")
            .await
            .unwrap();

        let all = store.list_sessions().await.unwrap();
        assert_eq!(all.len(), 4);
        let proj_a = store.list_sessions_filtered(Some(&p1.id)).await.unwrap();
        assert_eq!(proj_a.len(), 2);
        let proj_b = store.list_sessions_filtered(Some(&p2.id)).await.unwrap();
        assert_eq!(proj_b.len(), 1);
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn create_session_with_project_shows_in_session_count() {
        let cfg = temp_db("proj-count");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store.create_project("count-test", "/tmp").await.unwrap();
        assert_eq!(p.session_count, 0);
        store
            .create_session_with_project("sc1", "m", "t", "Coding Agent", Some(&p.id))
            .await
            .unwrap();
        let updated = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(updated.session_count, 1);
        cleanup(&cfg.db_path);
    }

    // ── Project edge cases ──

    #[tokio::test]
    async fn get_project_returns_none_for_unknown_id() {
        let cfg = temp_db("proj-none");
        let store = SessionStore::open(&cfg).await.unwrap();
        let result = store.get_project("nonexistent").await.unwrap();
        assert!(result.is_none());
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn get_project_returns_correct_data() {
        let cfg = temp_db("proj-get");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store
            .create_project("my-project", "/home/user/code")
            .await
            .unwrap();
        let fetched = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "my-project");
        assert_eq!(fetched.cwd, "/home/user/code");
        assert_eq!(fetched.session_count, 0);
        assert_eq!(fetched.id, p.id);
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn update_project_partial_name_only() {
        let cfg = temp_db("proj-upd-n");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store
            .create_project("original", "/original/path")
            .await
            .unwrap();
        store
            .update_project(&p.id, Some("renamed"), None)
            .await
            .unwrap();
        let fetched = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "renamed");
        assert_eq!(fetched.cwd, "/original/path"); // cwd unchanged
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn update_project_partial_cwd_only() {
        let cfg = temp_db("proj-upd-c");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store
            .create_project("keep-name", "/old/path")
            .await
            .unwrap();
        store
            .update_project(&p.id, None, Some("/new/path"))
            .await
            .unwrap();
        let fetched = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "keep-name"); // name unchanged
        assert_eq!(fetched.cwd, "/new/path");
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn update_project_no_changes_succeeds() {
        let cfg = temp_db("proj-upd-0");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store.create_project("stable", "/stable").await.unwrap();
        // Passing both as None should not error
        store.update_project(&p.id, None, None).await.unwrap();
        let fetched = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "stable");
        assert_eq!(fetched.cwd, "/stable");
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn delete_project_nonexistent_is_noop() {
        let cfg = temp_db("proj-del-no");
        let store = SessionStore::open(&cfg).await.unwrap();
        // Should not panic or error — just return 0 rows deleted
        let result = store.delete_project("nonexistent-id").await.unwrap();
        assert_eq!(result, 0);
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn delete_project_then_get_returns_none() {
        let cfg = temp_db("proj-del-get");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store.create_project("temp-project", "/tmp").await.unwrap();
        store.delete_project(&p.id).await.unwrap();
        let result = store.get_project(&p.id).await.unwrap();
        assert!(result.is_none());
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn list_projects_empty_returns_empty_vec() {
        let cfg = temp_db("proj-empty");
        let store = SessionStore::open(&cfg).await.unwrap();
        let projects = store.list_projects().await.unwrap();
        assert!(projects.is_empty());
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn list_sessions_without_project_filter_returns_all() {
        let cfg = temp_db("proj-all-sess");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p1 = store.create_project("p1", "/p1").await.unwrap();
        store
            .create_session_with_project("s-p1", "m", "task-p1", "Coding Agent", Some(&p1.id))
            .await
            .unwrap();
        store
            .create_session("s-no-proj", "m", "no-project", "Chat Agent")
            .await
            .unwrap();
        // list_sessions (without filter) returns all
        let all = store.list_sessions().await.unwrap();
        assert_eq!(all.len(), 2);
        // list_sessions_filtered(None) also returns all
        let all2 = store.list_sessions_filtered(None).await.unwrap();
        assert_eq!(all2.len(), 2);
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn list_sessions_filtered_by_wrong_project_returns_empty() {
        let cfg = temp_db("proj-wrong");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p1 = store.create_project("p1", "/p1").await.unwrap();
        let p2 = store.create_project("p2", "/p2").await.unwrap();
        store
            .create_session_with_project("s1", "m", "t1", "Coding Agent", Some(&p1.id))
            .await
            .unwrap();
        // Query p2 sessions — should be empty since s1 belongs to p1
        let p2_sessions = store.list_sessions_filtered(Some(&p2.id)).await.unwrap();
        assert!(p2_sessions.is_empty());
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn multiple_projects_sessions_isolated() {
        let cfg = temp_db("proj-iso");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p_a = store.create_project("proj-a", "/a").await.unwrap();
        let p_b = store.create_project("proj-b", "/b").await.unwrap();

        // Create sessions in interleaved order
        store
            .create_session_with_project("a1", "m", "task-a1", "Coding Agent", Some(&p_a.id))
            .await
            .unwrap();
        store
            .create_session_with_project("b1", "m", "task-b1", "Coding Agent", Some(&p_b.id))
            .await
            .unwrap();
        store
            .create_session_with_project("a2", "m", "task-a2", "Coding Agent", Some(&p_a.id))
            .await
            .unwrap();
        store
            .create_session_with_project("b2", "m", "task-b2", "Coding Agent", Some(&p_b.id))
            .await
            .unwrap();
        store
            .create_session_with_project("a3", "m", "task-a3", "Coding Agent", Some(&p_a.id))
            .await
            .unwrap();

        // Each project sees only its own sessions
        let a_sessions = store.list_sessions_filtered(Some(&p_a.id)).await.unwrap();
        let b_sessions = store.list_sessions_filtered(Some(&p_b.id)).await.unwrap();
        assert_eq!(a_sessions.len(), 3);
        assert_eq!(b_sessions.len(), 2);

        // All session IDs in a_sessions start with "a"
        for s in &a_sessions {
            assert!(
                s.id.starts_with('a'),
                "session {} should belong to project a",
                s.id
            );
        }
        for s in &b_sessions {
            assert!(
                s.id.starts_with('b'),
                "session {} should belong to project b",
                s.id
            );
        }

        // Project session counts are correct
        let pa = store.get_project(&p_a.id).await.unwrap().unwrap();
        let pb = store.get_project(&p_b.id).await.unwrap().unwrap();
        assert_eq!(pa.session_count, 3);
        assert_eq!(pb.session_count, 2);

        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn create_session_defaults_to_no_project() {
        let cfg = temp_db("proj-def");
        let store = SessionStore::open(&cfg).await.unwrap();
        // create_session (the convenience method) creates a session with no project_id
        store
            .create_session("s-no-proj", "m", "just a task", "Chat Agent")
            .await
            .unwrap();
        let all = store.list_sessions().await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(
            all[0].project_id.is_none(),
            "session created without project should have null project_id"
        );
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn update_project_name_to_existing_name_is_allowed() {
        // Different projects can have same name (name is not UNIQUE)
        let cfg = temp_db("proj-dup");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p1 = store.create_project("same-name", "/path1").await.unwrap();
        let p2 = store.create_project("same-name", "/path2").await.unwrap();
        let projects = store.list_projects().await.unwrap();
        assert_eq!(projects.len(), 2);
        // Update p1's name to something new — should work
        store
            .update_project(&p1.id, Some("new-unique-name"), None)
            .await
            .unwrap();
        let updated = store.get_project(&p1.id).await.unwrap().unwrap();
        assert_eq!(updated.name, "new-unique-name");
        // p2 still has old name
        let p2_still = store.get_project(&p2.id).await.unwrap().unwrap();
        assert_eq!(p2_still.name, "same-name");
        cleanup(&cfg.db_path);
    }

    // ── More edge cases ──

    #[tokio::test]
    async fn create_project_with_special_chars_in_name() {
        let cfg = temp_db("proj-special");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store
            .create_project("my-app_v2.0 (test)", "/home/user/projects/my-app_v2.0")
            .await
            .unwrap();
        assert_eq!(p.name, "my-app_v2.0 (test)");
        let fetched = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "my-app_v2.0 (test)");
        assert_eq!(fetched.cwd, "/home/user/projects/my-app_v2.0");
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn create_session_with_explicit_null_project() {
        let cfg = temp_db("proj-null");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session_with_project("sn", "m", "t", "Chat Agent", None)
            .await
            .unwrap();
        let sessions = store.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].project_id.is_none());
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn project_create_update_delete_full_cycle() {
        let cfg = temp_db("proj-cycle");
        let store = SessionStore::open(&cfg).await.unwrap();
        // Create
        let p = store
            .create_project("cycle-test", "/tmp/cycle")
            .await
            .unwrap();
        assert!(store.get_project(&p.id).await.unwrap().is_some());
        // Update
        store
            .update_project(&p.id, Some("cycle-v2"), Some("/tmp/cycle-v2"))
            .await
            .unwrap();
        let updated = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(updated.name, "cycle-v2");
        assert_eq!(updated.cwd, "/tmp/cycle-v2");
        // Delete
        let deleted = store.delete_project(&p.id).await.unwrap();
        assert!(deleted >= 1);
        assert!(store.get_project(&p.id).await.unwrap().is_none());
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn project_cwd_can_be_non_existent_path() {
        // cwd is just a string — it doesn't need to exist on disk
        let cfg = temp_db("proj-cwd");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store
            .create_project("no-disk", "/this/path/does/not/exist")
            .await
            .unwrap();
        let fetched = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(fetched.cwd, "/this/path/does/not/exist");
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn orphaned_sessions_survive_project_deletion() {
        // When a project is deleted, sessions with project_id = deleted project
        // are also deleted (cascade via Rust code, not SQL CASCADE since we used ON DELETE SET NULL)
        let cfg = temp_db("proj-orphan");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store.create_project("to-delete", "/tmp").await.unwrap();
        store
            .create_session_with_project("s-orphan", "m", "t", "Coding Agent", Some(&p.id))
            .await
            .unwrap();
        // Delete project — should cascade-delete session via Rust code
        store.delete_project(&p.id).await.unwrap();
        let sessions = store.list_sessions().await.unwrap();
        assert!(
            !sessions.iter().any(|s| s.id == "s-orphan"),
            "session should be deleted with project"
        );
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn list_sessions_default_uses_list_sessions_compat() {
        // list_sessions() without args should call list_sessions_filtered(None)
        let cfg = temp_db("proj-compat");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s1", "m", "t1", "Agent")
            .await
            .unwrap();
        store
            .create_session("s2", "m", "t2", "Agent")
            .await
            .unwrap();
        let all = store.list_sessions().await.unwrap();
        assert_eq!(all.len(), 2);
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn session_summary_includes_all_fields() {
        let cfg = temp_db("proj-summary");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store.create_project("proj", "/proj").await.unwrap();
        store
            .create_session_with_project(
                "s-full",
                "deepseek-v4",
                "A test task",
                "Coding Agent",
                Some(&p.id),
            )
            .await
            .unwrap();

        let sessions = store.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.id, "s-full");
        assert_eq!(s.model, "deepseek-v4");
        assert_eq!(s.task, "A test task");
        assert_eq!(s.agent_name.as_deref(), Some("Coding Agent"));
        assert_eq!(s.project_id.as_deref(), Some(p.id.as_str()));
        assert_eq!(s.turns, 0);
        assert_eq!(s.status, "active");
        cleanup(&cfg.db_path);
    }

    // ── E2E integration tests ──

    #[tokio::test]
    async fn full_project_lifecycle_with_sessions() {
        // Simulates a real user workflow: create project → create sessions → update → delete
        let cfg = temp_db("e2e-lifecycle");
        let store = SessionStore::open(&cfg).await.unwrap();

        // 1. Create a project
        let p = store
            .create_project("my-coding-project", "/Users/dev/code")
            .await
            .unwrap();
        assert_eq!(p.session_count, 0);

        // 2. Create multiple coding sessions in the project
        store
            .create_session_with_project(
                "s1",
                "deepseek-v4",
                "Fix authentication bug",
                "Coding Agent",
                Some(&p.id),
            )
            .await
            .unwrap();
        store
            .create_session_with_project(
                "s2",
                "deepseek-v4",
                "Add unit tests for UserService",
                "Coding Agent",
                Some(&p.id),
            )
            .await
            .unwrap();
        store
            .create_session_with_project(
                "s3",
                "deepseek-v4-pro",
                "Refactor database layer",
                "Coding Agent",
                Some(&p.id),
            )
            .await
            .unwrap();

        // 3. Also create a Chat Agent session (not in project)
        store
            .create_session("s-chat", "deepseek-v4", "General question", "Chat Agent")
            .await
            .unwrap();

        // 4. Verify project sees only its sessions
        let p_sessions = store.list_sessions_filtered(Some(&p.id)).await.unwrap();
        assert_eq!(p_sessions.len(), 3);
        for s in &p_sessions {
            assert_eq!(s.agent_name.as_deref(), Some("Coding Agent"));
            assert_eq!(s.project_id.as_deref(), Some(p.id.as_str()));
        }

        // 5. Verify project session count updated
        let updated = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(updated.session_count, 3);

        // 6. Update project
        store
            .update_project(
                &p.id,
                Some("my-coding-project-v2"),
                Some("/Users/dev/code-v2"),
            )
            .await
            .unwrap();
        let updated = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(updated.name, "my-coding-project-v2");
        assert_eq!(updated.cwd, "/Users/dev/code-v2");

        // 7. Delete all project sessions
        store.delete_session("s1").await.unwrap();
        store.delete_session("s2").await.unwrap();

        // 8. Verify remaining sessions
        let remaining = store.list_sessions_filtered(Some(&p.id)).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "s3");

        // 9. Delete project → cascades remaining session
        store.delete_project(&p.id).await.unwrap();
        assert!(store.get_project(&p.id).await.unwrap().is_none());
        let after_delete = store.list_sessions_filtered(Some(&p.id)).await.unwrap();
        assert!(after_delete.is_empty());

        // 10. Chat session still exists
        let all = store.list_sessions().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "s-chat");

        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn cross_project_session_isolation() {
        // Two projects, interleaved sessions — verify strict isolation
        let cfg = temp_db("e2e-isolation");
        let store = SessionStore::open(&cfg).await.unwrap();

        let frontend = store
            .create_project("frontend", "/code/frontend")
            .await
            .unwrap();
        let backend = store
            .create_project("backend", "/code/backend")
            .await
            .unwrap();

        // Interleave session creation
        store
            .create_session_with_project(
                "fe-1",
                "m",
                "Add login page",
                "Coding Agent",
                Some(&frontend.id),
            )
            .await
            .unwrap();
        store
            .create_session_with_project(
                "be-1",
                "m",
                "Add auth API",
                "Coding Agent",
                Some(&backend.id),
            )
            .await
            .unwrap();
        store
            .create_session_with_project(
                "fe-2",
                "m",
                "Add dashboard",
                "Coding Agent",
                Some(&frontend.id),
            )
            .await
            .unwrap();
        store
            .create_session_with_project(
                "be-2",
                "m",
                "Add DB migration",
                "Coding Agent",
                Some(&backend.id),
            )
            .await
            .unwrap();
        store
            .create_session_with_project(
                "fe-3",
                "m",
                "Fix CSS bug",
                "Coding Agent",
                Some(&frontend.id),
            )
            .await
            .unwrap();

        // Frontend: 3 sessions
        let fe_sessions = store
            .list_sessions_filtered(Some(&frontend.id))
            .await
            .unwrap();
        assert_eq!(fe_sessions.len(), 3);

        // Backend: 2 sessions
        let be_sessions = store
            .list_sessions_filtered(Some(&backend.id))
            .await
            .unwrap();
        assert_eq!(be_sessions.len(), 2);

        // Session IDs are correctly partitioned
        let fe_ids: Vec<&str> = fe_sessions.iter().map(|s| s.id.as_str()).collect();
        let be_ids: Vec<&str> = be_sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(fe_ids.contains(&"fe-1"));
        assert!(fe_ids.contains(&"fe-2"));
        assert!(fe_ids.contains(&"fe-3"));
        assert!(be_ids.contains(&"be-1"));
        assert!(be_ids.contains(&"be-2"));

        // Delete frontend project → only frontend sessions are removed
        store.delete_project(&frontend.id).await.unwrap();
        let all_after = store.list_sessions().await.unwrap();
        assert_eq!(all_after.len(), 2);
        assert!(all_after.iter().all(|s| s.id.starts_with("be-")));

        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn session_messages_survive_project_update() {
        // Updating a project should not affect its sessions or messages
        let cfg = temp_db("e2e-update-safe");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store
            .create_project("test-proj", "/tmp/test")
            .await
            .unwrap();
        store
            .create_session_with_project(
                "s-msg",
                "m",
                "Important task",
                "Coding Agent",
                Some(&p.id),
            )
            .await
            .unwrap();

        // Save some messages
        let u1 = Message::user("Write a test");
        let a1 = Message::assistant("Here is a test...");
        let batch: Vec<(usize, &Message)> = vec![(0, &u1), (1, &a1)];
        store.save_messages_batch("s-msg", &batch).await.unwrap();

        // Update project
        store
            .update_project(&p.id, Some("renamed-project"), Some("/new/path"))
            .await
            .unwrap();

        // Messages should still be there
        let msgs = store.get_recent_messages("s-msg", 10).await.unwrap();
        assert_eq!(msgs.len(), 2);

        // Session still belongs to project
        let sessions = store.list_sessions_filtered(Some(&p.id)).await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "s-msg");

        cleanup(&cfg.db_path);
    }

    // ── Workspace integration tests ──

    #[tokio::test]
    async fn project_cwd_persists_and_retrievable() {
        let cfg = temp_db("ws-cwd");
        let store = SessionStore::open(&cfg).await.unwrap();
        let cwd = "/home/user/projects/my-coding-project";
        let p = store.create_project("my-project", cwd).await.unwrap();

        // Verify cwd is stored correctly
        let fetched = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(fetched.cwd, cwd);

        // Update cwd
        let new_cwd = "/home/user/projects/my-project-v2";
        store
            .update_project(&p.id, None, Some(new_cwd))
            .await
            .unwrap();
        let updated = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(updated.cwd, new_cwd);
        assert_eq!(updated.name, "my-project"); // name unchanged

        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn multiple_projects_different_cwds() {
        let cfg = temp_db("ws-multi");
        let store = SessionStore::open(&cfg).await.unwrap();

        let frontend = store
            .create_project("frontend", "/code/frontend")
            .await
            .unwrap();
        let backend = store
            .create_project("backend", "/code/backend")
            .await
            .unwrap();

        let fe = store.get_project(&frontend.id).await.unwrap().unwrap();
        let be = store.get_project(&backend.id).await.unwrap().unwrap();

        assert_eq!(fe.cwd, "/code/frontend");
        assert_eq!(be.cwd, "/code/backend");
        assert_ne!(fe.cwd, be.cwd);

        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn project_cwd_unchanged_after_session_operations() {
        // Creating/deleting sessions should not affect project cwd
        let cfg = temp_db("ws-stable");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store.create_project("stable", "/stable/cwd").await.unwrap();

        store
            .create_session_with_project("s1", "m", "t1", "Coding Agent", Some(&p.id))
            .await
            .unwrap();
        store
            .create_session_with_project("s2", "m", "t2", "Coding Agent", Some(&p.id))
            .await
            .unwrap();
        store.delete_session("s1").await.unwrap();

        let fetched = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(fetched.cwd, "/stable/cwd");
        assert_eq!(fetched.session_count, 1); // one session remaining

        cleanup(&cfg.db_path);
    }

    // ── Rename project tests ──

    #[tokio::test]
    async fn rename_project_updates_name_only() {
        let cfg = temp_db("rename-1");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store.create_project("old-name", "/some/cwd").await.unwrap();

        store
            .update_project(&p.id, Some("new-name"), None)
            .await
            .unwrap();
        let fetched = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "new-name");
        assert_eq!(fetched.cwd, "/some/cwd"); // cwd unchanged
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn rename_project_to_same_name_is_noop() {
        let cfg = temp_db("rename-same");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store.create_project("my-project", "/tmp").await.unwrap();

        store
            .update_project(&p.id, Some("my-project"), None)
            .await
            .unwrap();
        let fetched = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "my-project");
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn rename_project_empty_name_is_stored() {
        let cfg = temp_db("rename-empty");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store.create_project("has-name", "/tmp").await.unwrap();

        // Empty string is technically allowed (frontend validates)
        store.update_project(&p.id, Some(""), None).await.unwrap();
        let fetched = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "");
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn rename_project_does_not_affect_other_projects() {
        let cfg = temp_db("rename-iso");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p1 = store.create_project("alpha", "/a").await.unwrap();
        let p2 = store.create_project("beta", "/b").await.unwrap();

        store
            .update_project(&p1.id, Some("alpha-v2"), None)
            .await
            .unwrap();

        let p1_fetched = store.get_project(&p1.id).await.unwrap().unwrap();
        let p2_fetched = store.get_project(&p2.id).await.unwrap().unwrap();
        assert_eq!(p1_fetched.name, "alpha-v2");
        assert_eq!(p2_fetched.name, "beta"); // unchanged
        cleanup(&cfg.db_path);
    }

    // ── Message format integrity tests ──

    #[tokio::test]
    async fn messages_saved_with_correct_order() {
        let cfg = temp_db("msg-order");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s1", "m", "task", "Agent")
            .await
            .unwrap();

        let sys = Message::system("System prompt");
        let user = Message::user("Hello");
        let assistant = Message::assistant("Hi there");
        let tool_result = Message::tool_result("t1", "output", false);
        let batch: Vec<(usize, &Message)> =
            vec![(0, &sys), (0, &user), (0, &assistant), (0, &tool_result)];
        store.save_messages_batch("s1", &batch).await.unwrap();

        let msgs = store.get_recent_messages("s1", 10).await.unwrap();
        assert_eq!(msgs.len(), 4);
        assert!(
            matches!(msgs[0].role, Role::System),
            "first should be system"
        );
        assert!(matches!(msgs[1].role, Role::User), "second should be user");
        assert!(
            matches!(msgs[2].role, Role::Assistant),
            "third should be assistant"
        );
        assert!(matches!(msgs[3].role, Role::Tool), "fourth should be tool");
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn tool_use_and_result_are_paired_in_session() {
        let cfg = temp_db("msg-pair");
        let store = SessionStore::open(&cfg).await.unwrap();
        store.create_session("s2", "m", "t", "Agent").await.unwrap();

        let assistant_with_tool = Message {
            meta: None,
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "let me read".into(),
                },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path":"a.txt"}),
                },
            ],
        };
        let tool_result = Message::tool_result("t1", "file content", false);
        let batch: Vec<(usize, &Message)> = vec![(0, &assistant_with_tool), (0, &tool_result)];
        store.save_messages_batch("s2", &batch).await.unwrap();

        let msgs = store.get_recent_messages("s2", 10).await.unwrap();
        assert_eq!(msgs.len(), 2);

        // Verify tool_use and tool_result share the same ID
        let use_ids: Vec<&str> = msgs[0]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        let result_ids: Vec<&str> = msgs[1]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(use_ids.len(), 1);
        assert_eq!(result_ids.len(), 1);
        assert_eq!(
            use_ids[0], result_ids[0],
            "tool_use and tool_result IDs must match"
        );
        cleanup(&cfg.db_path);
    }

    // ── Referential integrity tests ──

    #[tokio::test]
    async fn session_rejects_nonexistent_project() {
        let cfg = temp_db("ref-null");
        let store = SessionStore::open(&cfg).await.unwrap();
        // FK constraint prevents creating a session with non-existent project
        let result = store
            .create_session_with_project(
                "s-orphan",
                "m",
                "t",
                "Coding Agent",
                Some("nonexistent-proj"),
            )
            .await;
        assert!(result.is_err(), "FK must reject non-existent project_id");
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn project_deletion_cleans_related_sessions() {
        let cfg = temp_db("ref-clean");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store.create_project("proj", "/tmp").await.unwrap();
        store
            .create_session_with_project("s1", "m", "t1", "Coding Agent", Some(&p.id))
            .await
            .unwrap();
        store
            .create_session_with_project("s2", "m", "t2", "Coding Agent", Some(&p.id))
            .await
            .unwrap();

        assert_eq!(
            store
                .list_sessions_filtered(Some(&p.id))
                .await
                .unwrap()
                .len(),
            2
        );
        store.delete_project(&p.id).await.unwrap();
        // Sessions in project should be deleted
        assert_eq!(
            store
                .list_sessions_filtered(Some(&p.id))
                .await
                .unwrap()
                .len(),
            0
        );
        // Project itself is gone
        assert!(store.get_project(&p.id).await.unwrap().is_none());
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn sessions_without_project_unaffected_by_project_deletion() {
        let cfg = temp_db("ref-safe");
        let store = SessionStore::open(&cfg).await.unwrap();
        let p = store.create_project("proj", "/tmp").await.unwrap();
        store
            .create_session_with_project("s-linked", "m", "t1", "Coding Agent", Some(&p.id))
            .await
            .unwrap();
        store
            .create_session("s-free", "m", "t2", "Chat Agent")
            .await
            .unwrap();

        store.delete_project(&p.id).await.unwrap();
        let all = store.list_sessions().await.unwrap();
        assert_eq!(all.len(), 1, "only non-project session should survive");
        assert_eq!(all[0].id, "s-free");
        cleanup(&cfg.db_path);
    }

    // ── Stage-1 integration tests: compact_session_messages (manual DB-level compact) ──

    fn asst_with_tool_use(id: &str) -> Message {
        Message {
            meta: None,
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: "read".into(),
                input: serde_json::json!({"path":"a.txt"}),
            }],
        }
    }

    #[tokio::test]
    async fn compact_session_preserves_initial_goal() {
        let cfg = temp_db("compact-goal");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s-goal", "m", "t", "Agent")
            .await
            .unwrap();

        let u1 = Message::user("initial goal");
        let a1 = Message::assistant("a1");
        let u2 = Message::user("u2");
        let a2 = Message::assistant("a2");
        let u3 = Message::user("u3");
        let a3 = Message::assistant("a3");
        let batch: Vec<(usize, &Message)> =
            vec![(0, &u1), (1, &a1), (2, &u2), (3, &a2), (4, &u3), (5, &a3)];
        store.save_messages_batch("s-goal", &batch).await.unwrap();

        let (before, after) = store.compact_session_messages("s-goal", 1).await.unwrap();
        assert!(
            after < before,
            "manual compact must shrink tokens: {before} -> {after}"
        );

        let msgs = store.get_recent_messages("s-goal", 100).await.unwrap();
        // anchors (goal + recent users u2,u3) + 1 recent non-anchor (a3)
        assert_eq!(
            msgs.len(),
            4,
            "goal + u2 + u3 + a3 expected, got {}",
            msgs.len()
        );
        match &msgs[0].content[0] {
            ContentBlock::Text { text } => {
                assert_eq!(text, "initial goal", "initial goal must be first")
            }
            _ => panic!("expected Text"),
        }
        // initial goal never summarized away
        let goal_present = msgs.iter().any(
            |m| matches!(&m.content[0], ContentBlock::Text { text } if text == "initial goal"),
        );
        assert!(
            goal_present,
            "initial goal must survive compact_session_messages"
        );
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn compact_session_preserves_system_prompt() {
        let cfg = temp_db("compact-sys");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s-sys", "m", "t", "Agent")
            .await
            .unwrap();

        let sys = Message::system("SYSTEM_PROMPT");
        let u1 = Message::user("goal");
        let a1 = Message::assistant("a1");
        let u2 = Message::user("u2");
        let a2 = Message::assistant("a2");
        let batch: Vec<(usize, &Message)> = vec![(0, &sys), (0, &u1), (1, &a1), (2, &u2), (3, &a2)];
        store.save_messages_batch("s-sys", &batch).await.unwrap();

        store.compact_session_messages("s-sys", 1).await.unwrap();
        let msgs = store.get_recent_messages("s-sys", 100).await.unwrap();
        assert!(
            matches!(msgs[0].role, Role::System),
            "system prompt must stay first"
        );
        match &msgs[0].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "SYSTEM_PROMPT"),
            _ => panic!("expected Text"),
        }
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn compact_session_keeps_tool_pair_integrity() {
        // Old closed tool pair (asst t1 + result t1) sits outside the recent window.
        // Manual compact must drop BOTH halves together, never leave an orphan result.
        let cfg = temp_db("compact-pair");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s-pair", "m", "t", "Agent")
            .await
            .unwrap();

        let u1 = Message::user("goal");
        let asst_tool = asst_with_tool_use("t1");
        let tool_res = Message::tool_result("t1", "content", false);
        let a3 = Message::assistant("a3");
        let a4 = Message::assistant("a4");
        let batch: Vec<(usize, &Message)> = vec![
            (0, &u1),
            (0, &asst_tool),
            (0, &tool_res),
            (1, &a3),
            (2, &a4),
        ];
        store.save_messages_batch("s-pair", &batch).await.unwrap();

        store.compact_session_messages("s-pair", 2).await.unwrap();
        let msgs = store.get_recent_messages("s-pair", 100).await.unwrap();
        let tool_uses = msgs
            .iter()
            .flat_map(|m| &m.content)
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .count();
        let tool_results = msgs
            .iter()
            .flat_map(|m| &m.content)
            .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
            .count();
        assert_eq!(tool_uses, tool_results,
            "compact must not leave an orphan tool half: {tool_uses} uses vs {tool_results} results");
        // initial goal still present
        assert!(msgs
            .iter()
            .any(|m| matches!(&m.content[0], ContentBlock::Text { text } if text == "goal")));
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn compact_session_reduces_big_tool_output() {
        // The big old tool result should be dropped (with its assistant), so the token
        // estimate after compact is well below before.
        let cfg = temp_db("compact-shrink");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s-shrink", "m", "t", "Agent")
            .await
            .unwrap();

        let big = "x".repeat(10_000);
        let u1 = Message::user("goal");
        let asst_tool = asst_with_tool_use("t1");
        let tool_res = Message::tool_result("t1", &big, false);
        let tail: Vec<Message> = (0..6)
            .map(|i| Message::assistant(format!("a{i}").as_str()))
            .collect();
        let mut batch: Vec<(usize, &Message)> = vec![(0, &u1), (0, &asst_tool), (0, &tool_res)];
        for (i, m) in tail.iter().enumerate() {
            batch.push((i as usize + 1, m));
        }
        store.save_messages_batch("s-shrink", &batch).await.unwrap();

        let (before, after) = store.compact_session_messages("s-shrink", 2).await.unwrap();
        assert!(after < before, "compact must shrink: {before} -> {after}");
        assert!(
            before > 1000,
            "big tool output must dominate before-estimate ({before})"
        );
        assert!(
            after < 1000,
            "big tool output must be gone after compact ({after})"
        );

        let msgs = store.get_recent_messages("s-shrink", 100).await.unwrap();
        assert!(
            msgs.len() < 9,
            "must drop old tool pair and middle assistants, got {}",
            msgs.len()
        );
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn compact_session_empty_session_ok() {
        let cfg = temp_db("compact-empty");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s-empty", "m", "t", "Agent")
            .await
            .unwrap();

        let (before, after) = store.compact_session_messages("s-empty", 2).await.unwrap();
        assert_eq!((before, after), (0, 0), "empty session compacts to (0, 0)");
        let msgs = store.get_recent_messages("s-empty", 100).await.unwrap();
        assert!(msgs.is_empty());
        cleanup(&cfg.db_path);
    }

    /// Read the text of all messages for a session (in insertion order).
    async fn texts(store: &SessionStore, sid: &str) -> Vec<String> {
        store
            .get_session_history(sid, 1000)
            .await
            .unwrap()
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .find_map(|c| match c {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default()
            })
            .collect()
    }

    #[tokio::test]
    async fn fork_session_copies_history_before_cutoff() {
        let cfg = temp_db("fork-copy");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("src", "m", "t", "Agent")
            .await
            .unwrap();
        let u1 = Message::user("u1");
        let a1 = Message::assistant("a1");
        let u2 = Message::user("u2");
        let a2 = Message::assistant("a2");
        store
            .save_messages_batch("src", &vec![(0, &u1), (1, &a1), (2, &u2), (3, &a2)])
            .await
            .unwrap();

        // Use u2's message id as the truncation point: the new thread should only contain u1/a1 (id < before)
        let u2_id = {
            let db = store.db.lock().await;
            db.query_row(
                "SELECT id FROM messages WHERE session_id='src' AND content_json LIKE '%u2%'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };

        store
            .fork_session("src", "fork", u2_id, "m", "fork-task", "Agent", None)
            .await
            .unwrap();
        let fork_texts = texts(&store, "fork").await;
        assert_eq!(
            fork_texts,
            vec!["u1", "a1"],
            "fork copies only messages before the cutoff"
        );
        // The source session stays append-only
        assert_eq!(texts(&store, "src").await.len(), 4);
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn fork_session_records_parent_and_fork_point() {
        let cfg = temp_db("fork-parent");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("src", "m", "t", "Agent")
            .await
            .unwrap();
        let u1 = Message::user("u1");
        store
            .save_messages_batch("src", &vec![(0, &u1)])
            .await
            .unwrap();
        let u1_id = {
            let db = store.db.lock().await;
            db.query_row("SELECT id FROM messages WHERE session_id='src'", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        };

        store
            .fork_session("src", "fork", u1_id, "m", "fork-task", "Agent", None)
            .await
            .unwrap();
        let sessions = store.list_sessions().await.unwrap();
        let fork = sessions.iter().find(|s| s.id == "fork").unwrap();
        assert_eq!(fork.parent_id.as_deref(), Some("src"));
        assert_eq!(fork.fork_point, Some(u1_id));
        assert_eq!(fork.task, "fork-task");
        let src = sessions.iter().find(|s| s.id == "src").unwrap();
        assert!(src.parent_id.is_none(), "source has no parent");
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn fork_session_missing_source_errors() {
        let cfg = temp_db("fork-missing");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("other", "m", "t", "Agent")
            .await
            .unwrap();
        let r = store
            .fork_session("nope", "fork", 1, "m", "t", "Agent", None)
            .await;
        assert!(r.is_err(), "forking a missing source must error");
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn fork_session_tool_blocks_are_copied() {
        let cfg = temp_db("fork-tool");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("src", "m", "t", "Agent")
            .await
            .unwrap();
        let asst_tool = asst_with_tool_use("t1");
        let tool_res = Message::tool_result("t1", "result-data", false);
        let u1 = Message::user("u1");
        store
            .save_messages_batch("src", &vec![(0, &u1), (0, &asst_tool), (0, &tool_res)])
            .await
            .unwrap();
        // Truncation point is after the last message: the whole turn (including ToolUse/ToolResult) should be copied
        let last_id = {
            let db = store.db.lock().await;
            db.query_row(
                "SELECT MAX(id) FROM messages WHERE session_id='src'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };

        store
            .fork_session("src", "fork", last_id + 1, "m", "fork-task", "Agent", None)
            .await
            .unwrap();
        let hist = store.get_session_history("fork", 100).await.unwrap();
 // ToolUse + ToolResult conversation context is copied together with messages (equivalent to fork rollout items)
        assert_eq!(hist.len(), 3, "u1 + tool pair before cutoff all copied");
        assert!(matches!(hist[0].role, Role::User));
        assert!(matches!(hist[1].role, Role::Assistant));
        assert!(matches!(hist[2].role, Role::Tool));

        // Truncate before u1 (u1's id): 0 messages copied
        let u1_id = {
            let db = store.db.lock().await;
            db.query_row(
                "SELECT id FROM messages WHERE session_id='src' AND content_json LIKE '%u1%'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };
        store
            .fork_session("src", "fork2", u1_id, "m", "fork2-task", "Agent", None)
            .await
            .unwrap();
        assert!(
            texts(&store, "fork2").await.is_empty(),
            "cutoff at u1's own id excludes u1"
        );
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn get_session_returns_summary_or_none() {
        let cfg = temp_db("get-session");
        let store = SessionStore::open(&cfg).await.unwrap();
        store.create_session("s1", "m", "t", "Agent").await.unwrap();
        let s = store.get_session("s1").await.unwrap().expect("present");
        assert_eq!(s.id, "s1");
        assert_eq!(s.task, "t");
        assert!(store.get_session("nope").await.unwrap().is_none());
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn get_session_history_with_ids_is_chronological() {
        let cfg = temp_db("history-ids");
        let store = SessionStore::open(&cfg).await.unwrap();
        store.create_session("s1", "m", "t", "Agent").await.unwrap();
        let u1 = Message::user("u1");
        let a1 = Message::assistant("a1");
        store
            .save_messages_batch("s1", &vec![(0, &u1), (0, &a1)])
            .await
            .unwrap();
        let hist = store.get_session_history_with_ids("s1", 100).await.unwrap();
        assert_eq!(hist.len(), 2);
        assert!(hist[0].0 < hist[1].0, "按插入 id 升序");
        assert!(matches!(hist[0].1.role, Role::User));
        assert!(matches!(hist[1].1.role, Role::Assistant));
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn delete_session_detaches_child_branches() {
        let cfg = temp_db("fork-delete");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("src", "m", "t", "Agent")
            .await
            .unwrap();
        let u1 = Message::user("u1");
        store
            .save_messages_batch("src", &vec![(0, &u1)])
            .await
            .unwrap();
        let u1_id = {
            let db = store.db.lock().await;
            db.query_row("SELECT id FROM messages WHERE session_id='src'", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        };
        store
            .fork_session("src", "child", u1_id, "m", "child-task", "Agent", None)
            .await
            .unwrap();

        store.delete_session("src").await.unwrap();
        let sessions = store.list_sessions().await.unwrap();
        assert!(!sessions.iter().any(|s| s.id == "src"), "source deleted");
        let child = sessions.iter().find(|s| s.id == "child").unwrap();
        assert!(
            child.parent_id.is_none(),
            "child promoted to root, not cascade-deleted"
        );
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn rename_session_sets_and_clears_title_override() {
        let cfg = temp_db("rename");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("s1", "m", "original task", "Agent")
            .await
            .unwrap();

        store.rename_session("s1", Some("我的命名")).await.unwrap();
        let s = store.get_session("s1").await.unwrap().unwrap();
        assert_eq!(s.title.as_deref(), Some("我的命名"), "override set");
        assert_eq!(s.task, "original task", "task untouched");

        store.rename_session("s1", None).await.unwrap();
        let s = store.get_session("s1").await.unwrap().unwrap();
        assert_eq!(
            s.title, None,
            "override cleared -> falls back to task-derived"
        );
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn archive_hides_from_default_list_but_stays_reachable() {
        let cfg = temp_db("archive-hide");
        let store = SessionStore::open(&cfg).await.unwrap();
        store
            .create_session("keep", "m", "t1", "Agent")
            .await
            .unwrap();
        store
            .create_session("gone", "m", "t2", "Agent")
            .await
            .unwrap();

        store.set_session_archived("gone", true).await.unwrap();

        let listed = store.list_sessions().await.unwrap();
        let ids: Vec<&str> = listed.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["keep"],
            "archived session excluded from default list"
        );

        // get_session does not filter archived sessions — restore/continuation still reachable
        let gone = store.get_session("gone").await.unwrap().unwrap();
        assert!(gone.archived_at.is_some(), "archived_at set");
        assert_eq!(
            gone.status, "active",
            "归档不改 status —— cleanup_old_sessions 按 status!=active 清理，归档会话不被误删"
        );
        cleanup(&cfg.db_path);
    }

    #[tokio::test]
    async fn list_archived_sessions_returns_only_archived_and_unarchive_restores() {
        let cfg = temp_db("archive-list");
        let store = SessionStore::open(&cfg).await.unwrap();
        store.create_session("a", "m", "t1", "Agent").await.unwrap();
        store.create_session("b", "m", "t2", "Agent").await.unwrap();
        store.set_session_archived("a", true).await.unwrap();

        let archived = store.list_archived_sessions().await.unwrap();
        let ids: Vec<&str> = archived.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["a"], "only archived listed");

        store.set_session_archived("a", false).await.unwrap();
        assert!(store.list_archived_sessions().await.unwrap().is_empty());
        assert!(
            store
                .list_sessions()
                .await
                .unwrap()
                .iter()
                .any(|s| s.id == "a"),
            "restored to default list"
        );
        cleanup(&cfg.db_path);
    }
}
