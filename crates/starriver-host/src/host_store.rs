//! Host-owned session metadata store: an authoritative workspace registry and
//! a fail-soft projection cache folded from session log events.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cordis_rslite::EffectHandle;
use rusqlite::Connection;
use starriver_kernel::json::JsonValue;
use starriver_kernel::session::SessionEventOccurred;

/// One workspace row: stable id over a canonical directory path plus the
/// manually ordered session account (`WorkspaceView`).
#[derive(Debug, Clone)]
pub struct WorkspaceView {
    pub id: String,
    pub path: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub session_ids: Vec<String>,
}

/// One projection-cache row folded from session log events. The cache is a
/// fail-soft read model: rows can be rebuilt from the log, so write failures
/// are warnings, never errors.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjcacheRow {
    pub session_id: String,
    pub title: Option<String>,
    pub title_source: Option<String>,
    pub workspace_id: Option<String>,
    pub archived_at: Option<i64>,
    pub message_count: u32,
    /// Agent the session started with (write-once stamp; None = legacy row).
    pub agent: Option<String>,
    /// Session origin marker (`"scheduled"` for scheduled-task runs; None =
    /// plain conversation). Drives the sidebar Tasks → Scheduled runs grouping.
    pub origin: Option<String>,
}

/// Host-owned session metadata store: workspace registry (authoritative) +
/// projection cache (fail-soft). Owns `<db>.host.db` with its own single
/// connection; the kernel persistence coordinator never touches this file.
pub struct HostStore {
    db: Arc<Mutex<Connection>>,
    _listener: EffectHandle,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl HostStore {
    /// Open (or create) the host store and register the bus projection
    /// listener for `session/event` occurrences.
    pub fn open(path: PathBuf, bus: cordis_rslite::EventBus) -> anyhow::Result<Arc<Self>> {
        // Fresh-install boot: the default db lives at `~/.starriver/…`, which
        // does not exist on a brand-new machine, and sqlite never creates
        // parent directories. Best-effort so open still surfaces real fs errors.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let db = Connection::open(&path)?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             -- WAL + NORMAL: consistent, and commits stop fsync-per-write
             -- (durability window widens to checkpoint intervals only).
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             -- Members cascade on group delete; do not rely on the
             -- SQLITE_DEFAULT_FOREIGN_KEYS build default.
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS workspaces (
               id         TEXT PRIMARY KEY,
               path       TEXT NOT NULL UNIQUE,
               title      TEXT NOT NULL,
               created_at INTEGER NOT NULL,
               updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS workspace_members (
               workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
               session_id   TEXT PRIMARY KEY,
               position     INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS skill_usage (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id  TEXT,
                project_id  TEXT,
                skill       TEXT NOT NULL,
                version     TEXT,
                mode        TEXT NOT NULL,
                ts          INTEGER NOT NULL
            );
             CREATE TABLE IF NOT EXISTS session_projcache (
               session_id    TEXT PRIMARY KEY,
               title         TEXT,
               title_source  TEXT,
               workspace_id  TEXT,
               archived_at   INTEGER,
               message_count INTEGER NOT NULL DEFAULT 0,
               seq_watermark INTEGER NOT NULL DEFAULT 0,
               agent         TEXT,
               origin        TEXT
             );",
        )?;
        // Additive migration for DBs created before the per-session execution
        // mode: ALTER on open (idempotent check via table_info).
        {
            let conn = Connection::open(&path)?;
            let has_col = conn
                .prepare("SELECT 1 FROM pragma_table_info('session_projcache') WHERE name = 'execution_mode'")?
                .exists([])?;
            if !has_col {
                conn.execute(
                    "ALTER TABLE session_projcache ADD COLUMN execution_mode TEXT",
                    [],
                )?;
            }
            let has_agent = conn
                .prepare(
                    "SELECT 1 FROM pragma_table_info('session_projcache') WHERE name = 'agent'",
                )?
                .exists([])?;
            if !has_agent {
                conn.execute("ALTER TABLE session_projcache ADD COLUMN agent TEXT", [])?;
            }
            // WI-152: scheduled-run origin stamp (Tasks → Scheduled runs grouping).
            let has_origin = conn
                .prepare("SELECT 1 FROM pragma_table_info('session_projcache') WHERE name = 'origin'")?
                .exists([])?;
            if !has_origin {
                conn.execute("ALTER TABLE session_projcache ADD COLUMN origin TEXT", [])?;
            }
        }
        let db = Arc::new(Mutex::new(db));

        let projection_db = Arc::clone(&db);
        let handle = bus.on_sync::<SessionEventOccurred, _>(move |occurred| {
            // Streaming deltas match no projection arm below, yet one turn
            // emits tens of thousands of them (2026-09-02 game002: 41,686 in
            // a 12-minute turn). Skipping them entirely avoids one autocommit
            // UPDATE per token on the agent's streaming thread. The watermark
            // only guards bump_message_count idempotency, and the next
            // non-chunk event re-advances it past the skipped seqs.
            if occurred.event.event_type.as_str() == "assistant/chunk" {
                return Ok(cordis_rslite::Value::new(()));
            }
            let session_id = occurred.session.id.0.clone();
            let seq = occurred.event.seq;
            let event_type = occurred.event.event_type.clone();
            let data = occurred.event.data.clone();
            let conn = projection_db.lock().expect("host store poisoned");
            let result = (|| -> Result<(), rusqlite::Error> {
                match event_type.as_str() {
                    "session/title" => {
                        let title = data
                            .get("title")
                            .and_then(JsonValue::as_str)
                            .map(String::from);
                        let source = data
                            .get("source")
                            .and_then(JsonValue::as_str)
                            .map(String::from);
                        upsert_title(&conn, &session_id, title, source)?;
                    }
                    "session/archived" => {
                        let archived = data
                            .get("archived")
                            .and_then(JsonValue::as_bool)
                            .unwrap_or(false);
                        upsert_archived(&conn, &session_id, archived)?;
                    }
                    "assistant/message" => {
                        bump_message_count(&conn, &session_id, seq)?;
                    }
                    _ => {}
                }
                advance_watermark(&conn, &session_id, seq)
            })();
            if let Err(error) = result {
                // Fail-soft: the projection is rebuildable from the log.
                tracing::warn!(
                    session = %session_id,
                    error = %error,
                    "host projection write failed"
                );
            }
            Ok(cordis_rslite::Value::new(()))
        });
        handle.forget();

        Ok(Arc::new(Self {
            db,
            _listener: EffectHandle::none(),
        }))
    }

    // ── workspace registry (authoritative) ──

    /// Sidebar-reserved group names: the synthetic Ungrouped / Tasks buckets
    /// own these labels, so workspaces and session titles may not claim them —
    /// a collision would render two identically-named rows.
    pub fn is_reserved_group_name(title: &str) -> bool {
        let t = title.trim();
        t.eq_ignore_ascii_case("ungrouped") || t.eq_ignore_ascii_case("tasks")
    }

    /// Idempotently resolve a workspace over an existing directory. Returns
    /// `(workspace_id, created)`. A missing or non-directory path fails;
    /// re-resolving an existing workspace returns it untouched (regrouping
    /// semantics — id == canonical path). A NEW workspace whose basename is a
    /// reserved sidebar name is rejected.
    pub fn resolve_workspace(&self, path: &str) -> Result<(String, bool), String> {
        let canonical = std::fs::canonicalize(path)
            .map_err(|_| "workspace path must be an existing directory".to_string())?;
        let canonical = canonical.to_string_lossy().into_owned();
        if !std::path::Path::new(&canonical).is_dir() {
            return Err("workspace path must be an existing directory".into());
        }
        let title = std::path::Path::new(&canonical)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&canonical)
            .to_string();
        let now = now_ms();
        let conn = self.db.lock().expect("host store poisoned");
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM workspaces WHERE id = ?1",
                [&canonical],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if exists {
            return Ok((canonical, false));
        }
        if Self::is_reserved_group_name(&title) {
            return Err(format!(
                "`{title}` is a reserved sidebar name — rename the directory or pick another path"
            ));
        }
        conn.execute(
            "INSERT INTO workspaces (id, path, title, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            rusqlite::params![canonical, canonical, title, now],
        )
        .map_err(|e| e.to_string())?;
        Ok((canonical, true))
    }

    pub fn rename_workspace(&self, id: &str, title: &str) -> Result<(), String> {
        let title = title.trim();
        if title.is_empty() {
            return Err("workspace title must be non-empty".into());
        }
        if Self::is_reserved_group_name(title) {
            return Err(format!("`{title}` is a reserved sidebar name"));
        }
        let conn = self.db.lock().expect("host store poisoned");
        let updated = conn
            .execute(
                "UPDATE workspaces SET title = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, title, now_ms()],
            )
            .map_err(|e| e.to_string())?;
        if updated == 0 {
            return Err("workspace not found".into());
        }
        Ok(())
    }

    pub fn delete_workspace(&self, id: &str) -> Result<(), String> {
        let conn = self.db.lock().expect("host store poisoned");
        conn.execute("DELETE FROM workspaces WHERE id = ?1", [id])
            .map_err(|e| e.to_string())?;
        // Sessions survive the delete. Their projcache workspace_id is kept as
        // a stale marker: workspace_id IS NULL means a Chat conversation (which
        // is never attached), while a stale id keeps the orphaned Coding
        // session out of Chat and under Coding's Ungrouped. Re-resolving the
        // same directory reuses the id (id == canonical path), which re-groups
        // the survivors automatically. The membership rows themselves go away
        // with the ON DELETE CASCADE.
        Ok(())
    }

    pub fn list_workspaces(&self) -> Vec<WorkspaceView> {
        let conn = self.db.lock().expect("host store poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT w.id, w.path, w.title, w.created_at, w.updated_at,
                        m.session_id
                 FROM workspaces w
                 LEFT JOIN workspace_members m ON m.workspace_id = w.id
                 ORDER BY w.created_at, m.position",
            )
            .expect("prepare workspaces");
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })
            .expect("query workspaces");
        let mut by_id: Vec<(WorkspaceView, Vec<String>)> = Vec::new();
        for row in rows.flatten() {
            let (id, path, title, created_at, updated_at, session_id) = row;
            if let Some(entry) = by_id.iter_mut().find(|(w, _)| w.id == id) {
                if let Some(session_id) = session_id {
                    entry.1.push(session_id);
                }
            } else {
                by_id.push((
                    WorkspaceView {
                        id: id.clone(),
                        path,
                        title,
                        created_at,
                        updated_at,
                        session_ids: Vec::new(),
                    },
                    session_id.into_iter().collect(),
                ));
            }
        }
        by_id
            .into_iter()
            .map(|(mut workspace, session_ids)| {
                workspace.session_ids = session_ids;
                workspace
            })
            .collect()
    }

    /// Attach a session to a workspace, prepending it to the manual order.
    pub fn attach_session(&self, workspace_id: &str, session_id: &str) -> Result<(), String> {
        let conn = self.db.lock().expect("host store poisoned");
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM workspace_members WHERE session_id = ?1",
            [session_id],
        )
        .map_err(|e| e.to_string())?;
        // BUG-62: the move must carry the projcache row with the member row
        // (attach/detach already maintain it), or the workspace lists and
        // the session projection disagree about where the session lives.
        tx.execute(
            "INSERT INTO session_projcache (session_id, workspace_id)
             VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET workspace_id = excluded.workspace_id",
            rusqlite::params![session_id, workspace_id],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE workspace_members SET position = position + 1 WHERE workspace_id = ?1",
            [workspace_id],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO workspace_members (workspace_id, session_id, position)
             VALUES (?1, ?2, 0)",
            rusqlite::params![workspace_id, session_id],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE session_projcache SET workspace_id = ?1 WHERE session_id = ?2",
            rusqlite::params![workspace_id, session_id],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT OR IGNORE INTO session_projcache (session_id, workspace_id)
             VALUES (?1, ?2)",
            rusqlite::params![session_id, workspace_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// Resolve the workspace for `path` and attach the session to it. A
    /// directory named like a sidebar group ("Ungrouped"/"Tasks") is never
    /// auto-registered — the synthetic buckets own those labels, so the run
    /// proceeds and the session stays in the synthetic Ungrouped bucket.
    pub fn attach_by_path(&self, path: &str, session_id: &str) -> Result<(), String> {
        let reserved = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .map(Self::is_reserved_group_name)
            .unwrap_or(false);
        if reserved {
            return Ok(());
        }
        let (workspace_id, _) = self.resolve_workspace(path)?;
        self.attach_session(&workspace_id, session_id)
    }

 /// Stamp a fresh session's execution mode (binding: chosen at creation,
    /// locked for the session's life). Write-once — a later stamp for the same
    /// session is ignored, so the first execute fixes the mode.
    pub fn stamp_execution_mode(&self, session_id: &str, mode: &str) -> Result<(), String> {
        let conn = self.db.lock().expect("host store poisoned");
        conn.execute(
            "INSERT INTO session_projcache (session_id, execution_mode) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET execution_mode = excluded.execution_mode
             WHERE session_projcache.execution_mode IS NULL",
            [session_id, mode],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Stamp the agent a session started with. Write-once: a continuing run
    /// never re-stamps, so legacy rows keep whatever first ran after the
    /// stamp existed and a session's agent never changes mid-life.
    pub fn stamp_session_agent(&self, session_id: &str, agent: &str) -> Result<(), String> {
        let conn = self.db.lock().expect("host store poisoned");
        conn.execute(
            "INSERT INTO session_projcache (session_id, agent) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET agent = excluded.agent
             WHERE session_projcache.agent IS NULL",
            [session_id, agent],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// WI-152: scheduled-run origin marker — drives the sidebar Tasks →
    /// Scheduled runs grouping. Idempotent; the value never changes.
    pub fn stamp_session_origin(&self, session_id: &str, origin: &str) -> Result<(), String> {
        let conn = self.db.lock().expect("host store poisoned");
        conn.execute(
            "INSERT INTO session_projcache (session_id, origin) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET origin = excluded.origin",
            [session_id, origin],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The session's locked execution mode; `None` for sessions created before
    /// the per-session binding existed (UI falls back to standard).
    pub fn execution_mode_of(&self, session_id: &str) -> Option<String> {
        let conn = self.db.lock().expect("host store poisoned");
        conn.query_row(
            "SELECT execution_mode FROM session_projcache WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    }

    /// Move `session_id` before `before` (or to the end when `None`).
    pub fn insert_before(
        &self,
        workspace_id: &str,
        session_id: &str,
        before: Option<&str>,
    ) -> Result<(), String> {
        let conn = self.db.lock().expect("host store poisoned");
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "DELETE FROM workspace_members WHERE session_id = ?1",
            [session_id],
        )
        .map_err(|e| e.to_string())?;
        // BUG-62: the move must carry the projcache row with the member row
        // (attach/detach already maintain it), or the workspace lists and
        // the session projection disagree about where the session lives.
        tx.execute(
            "INSERT INTO session_projcache (session_id, workspace_id)
             VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET workspace_id = excluded.workspace_id",
            rusqlite::params![session_id, workspace_id],
        )
        .map_err(|e| e.to_string())?;
        let target: i64 = match before {
            Some(before) => tx
                .query_row(
                    "SELECT position FROM workspace_members
                     WHERE workspace_id = ?1 AND session_id = ?2",
                    rusqlite::params![workspace_id, before],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?,
            None => tx
                .query_row(
                    "SELECT COALESCE(MAX(position) + 1, 0) FROM workspace_members
                     WHERE workspace_id = ?1",
                    [workspace_id],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?,
        };
        tx.execute(
            "UPDATE workspace_members SET position = position + 1
             WHERE workspace_id = ?1 AND position >= ?2",
            rusqlite::params![workspace_id, target],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO workspace_members (workspace_id, session_id, position)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![workspace_id, session_id, target],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    pub fn detach_session(&self, session_id: &str) -> Result<(), String> {
        let conn = self.db.lock().expect("host store poisoned");
        conn.execute(
            "DELETE FROM workspace_members WHERE session_id = ?1",
            [session_id],
        )
        .map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE session_projcache SET workspace_id = NULL WHERE session_id = ?1",
            [session_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    // ── projection cache read model ──

    pub fn list_sessions(&self, include_archived: bool) -> Vec<ProjcacheRow> {
        let conn = self.db.lock().expect("host store poisoned");
        let sql = if include_archived {
            "SELECT session_id, title, title_source, workspace_id, archived_at, message_count, agent, origin
             FROM session_projcache ORDER BY session_id"
        } else {
            "SELECT session_id, title, title_source, workspace_id, archived_at, message_count, agent, origin
             FROM session_projcache WHERE archived_at IS NULL ORDER BY session_id"
        };
        let mut stmt = conn.prepare(sql).expect("prepare projcache list");
        stmt.query_map([], |row| {
            Ok(ProjcacheRow {
                session_id: row.get(0)?,
                title: row.get(1)?,
                title_source: row.get(2)?,
                workspace_id: row.get(3)?,
                archived_at: row.get(4)?,
                message_count: row.get::<_, i64>(5)? as u32,
                agent: row.get(6)?,
                origin: row.get(7)?,
            })
        })
        .expect("query projcache")
        .flatten()
        .collect()
    }

    pub fn get(&self, session_id: &str) -> Option<ProjcacheRow> {
        let conn = self.db.lock().expect("host store poisoned");
        conn.query_row(
            "SELECT session_id, title, title_source, workspace_id, archived_at, message_count, agent, origin
             FROM session_projcache WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok(ProjcacheRow {
                    session_id: row.get(0)?,
                    title: row.get(1)?,
                    title_source: row.get(2)?,
                    workspace_id: row.get(3)?,
                    archived_at: row.get(4)?,
                    message_count: row.get::<_, i64>(5)? as u32,
                    agent: row.get(6)?,
                    origin: row.get(7)?,
                })
            },
        )
        .ok()
    }

    pub fn delete_session(&self, session_id: &str) -> Result<(), String> {
        let conn = self.db.lock().expect("host store poisoned");
        conn.execute(
            "DELETE FROM session_projcache WHERE session_id = ?1",
            [session_id],
        )
        .map_err(|e| e.to_string())?;
        conn.execute(
            "DELETE FROM workspace_members WHERE session_id = ?1",
            [session_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Make an Ungrouped session visible in listings without touching any
    /// member ordering (fork children of ungrouped parents; cold rebuilds).
    pub fn ensure_session_row(&self, session_id: &str) -> Result<(), String> {
        let conn = self.db.lock().expect("host store poisoned");
        conn.execute(
            "INSERT OR IGNORE INTO session_projcache (session_id) VALUES (?1)",
            [session_id],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Cold-rebuild write of one projection row folded from the durable log by
    /// the caller. The group is restored from the members table (which the row
    /// loss never touched); ordering is never disturbed.
    pub fn restore_session_row(
        &self,
        session_id: &str,
        title: Option<&str>,
        title_source: Option<&str>,
        archived_at: Option<i64>,
        message_count: u32,
        seq_watermark: i64,
    ) -> Result<(), String> {
        let conn = self.db.lock().expect("host store poisoned");
        let workspace_id: Option<String> = conn
            .query_row(
                "SELECT workspace_id FROM workspace_members WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .ok();
        // BUG-44: INSERT OR REPLACE drops the whole row on conflict — the
        // write-once execution_mode/agent stamps (not listed here) fell back
        // to NULL on every cold rebuild. Update only the folded projection
        // columns instead.
        conn.execute(
            "INSERT INTO session_projcache
               (session_id, title, title_source, workspace_id, archived_at, message_count, seq_watermark)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(session_id) DO UPDATE SET
               title = excluded.title,
               title_source = excluded.title_source,
               workspace_id = excluded.workspace_id,
               archived_at = excluded.archived_at,
               message_count = excluded.message_count,
               seq_watermark = excluded.seq_watermark",
            rusqlite::params![
                session_id,
                title,
                title_source,
                workspace_id,
                archived_at,
                message_count,
                seq_watermark
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Record a skill usage event (invocation/activation). Never fails the
    /// caller — ported from the legacy store during the A2a closeout.
    pub fn record_skill_usage(
        &self,
        session_id: Option<&str>,
        project_id: Option<&str>,
        skill: &str,
        version: Option<&str>,
        mode: &str,
    ) {
        let Ok(conn) = self.db.lock() else { return };
        let _ = conn.execute(
            "INSERT INTO skill_usage (session_id, project_id, skill, version, mode, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                session_id,
                project_id,
                skill,
                version,
                mode,
                chrono::Utc::now().timestamp()
            ],
        );
    }

    pub fn list_skill_usage(&self, limit: usize) -> Vec<crate::types::SkillUsageRecord> {
        let Ok(conn) = self.db.lock() else {
            return vec![];
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT session_id, project_id, skill, version, mode, ts
             FROM skill_usage ORDER BY ts DESC, id DESC LIMIT ?1",
        ) else {
            return vec![];
        };
        stmt.query_map(rusqlite::params![limit as i64], |row| {
            Ok(crate::types::SkillUsageRecord {
                session_id: row.get(0)?,
                project_id: row.get(1)?,
                skill: row.get(2)?,
                version: row.get(3)?,
                mode: row.get(4)?,
                ts: row.get(5)?,
            })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }
}

fn ensure_projcache_row(conn: &Connection, session_id: &str) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT OR IGNORE INTO session_projcache (session_id) VALUES (?1)",
        [session_id],
    )?;
    Ok(())
}

fn upsert_title(
    conn: &Connection,
    session_id: &str,
    title: Option<String>,
    source: Option<String>,
) -> Result<(), rusqlite::Error> {
    ensure_projcache_row(conn, session_id)?;
    let Some(title) = title else {
        // A user-sourced null title clears the override so fallback titles
        // (task-derived) apply again; non-user events never carry null.
        if source.as_deref() == Some("user") {
            conn.execute(
                "UPDATE session_projcache SET title = NULL, title_source = NULL
                 WHERE session_id = ?1",
                rusqlite::params![session_id],
            )?;
        }
        return Ok(());
    };
    let source = source.unwrap_or_else(|| "fallback".into());
    if source == "user" {
        conn.execute(
            "UPDATE session_projcache SET title = ?2, title_source = ?3 WHERE session_id = ?1",
            rusqlite::params![session_id, title, source],
        )?;
    } else {
        // fallback/provider only apply when the current title is not pinned
        // AND a provider (LLM-generated) title has not landed: re-stamping
        // the fallback with every new task text made the sidebar title churn
        // across the whole session (task → LLM title → next task → …).
        conn.execute(
            "UPDATE session_projcache SET title = ?2, title_source = ?3
             WHERE session_id = ?1
               AND COALESCE(title_source, '') NOT IN ('user', 'provider')",
            rusqlite::params![session_id, title, source],
        )?;
    }
    Ok(())
}

fn upsert_archived(
    conn: &Connection,
    session_id: &str,
    archived: bool,
) -> Result<(), rusqlite::Error> {
    ensure_projcache_row(conn, session_id)?;
    conn.execute(
        "UPDATE session_projcache SET archived_at = ?2 WHERE session_id = ?1",
        rusqlite::params![session_id, if archived { Some(now_ms()) } else { None }],
    )?;
    Ok(())
}

fn bump_message_count(
    conn: &Connection,
    session_id: &str,
    seq: u64,
) -> Result<(), rusqlite::Error> {
    ensure_projcache_row(conn, session_id)?;
    conn.execute(
        "UPDATE session_projcache SET message_count = message_count + 1
         WHERE session_id = ?1 AND seq_watermark < ?2",
        rusqlite::params![session_id, seq as i64],
    )?;
    Ok(())
}

fn advance_watermark(conn: &Connection, session_id: &str, seq: u64) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE session_projcache SET seq_watermark = MAX(seq_watermark, ?2)
         WHERE session_id = ?1",
        rusqlite::params![session_id, seq as i64],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_rslite::EventBus;
    use starriver_kernel::json::JsonValue;
    use starriver_kernel::session::{AppendOptions, Session, SessionId};

    fn title_event(title: &str, source: &str) -> JsonValue {
        JsonValue::Object(vec![
            ("title".into(), JsonValue::String(title.into())),
            ("source".into(), JsonValue::String(source.into())),
        ])
    }

    fn archived_event(archived: bool) -> JsonValue {
        JsonValue::Object(vec![("archived".into(), JsonValue::Bool(archived))])
    }

    fn assistant_event() -> JsonValue {
        JsonValue::Object(vec![(
            "message".into(),
            JsonValue::Object(vec![("content".into(), JsonValue::Array(vec![]))]),
        )])
    }

    fn chunk_event() -> JsonValue {
        JsonValue::Object(vec![(
            "chunk".into(),
            JsonValue::Object(vec![
                ("type".into(), JsonValue::String("reasoning-delta".into())),
                ("index".into(), JsonValue::UInt(0)),
                ("text".into(), JsonValue::String("think".into())),
            ]),
        )])
    }

    #[test]
    fn workspace_resolve_is_idempotent_and_titles_default_to_basename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("my-project");
        std::fs::create_dir_all(&path).unwrap();
        let store = HostStore::open(dir.path().join("host.db"), EventBus::new()).unwrap();

        let (id, created) = store.resolve_workspace(path.to_str().unwrap()).unwrap();
        assert!(created);
        let (id2, created2) = store.resolve_workspace(path.to_str().unwrap()).unwrap();
        assert!(!created2);
        assert_eq!(id, id2);

        let views = store.list_workspaces();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].title, "my-project");
        assert!(views[0].session_ids.is_empty());
    }

    #[test]
    fn workspace_resolve_rejects_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = HostStore::open(dir.path().join("host.db"), EventBus::new()).unwrap();
        assert!(store
            .resolve_workspace(dir.path().join("nope").to_str().unwrap())
            .is_err());
    }

    /// BUG-44 regression: `stamp_execution_mode` is documented write-once
    /// ("the first execute fixes the mode") and `restore_session_row` is the
    /// cold-rebuild path run by projection reconciliation on every session
    /// list. INSERT OR REPLACE dropped the unlisted execution_mode/agent
    /// columns back to NULL, silently un-fixing the mode (and letting a later
    /// stamp overwrite it, breaking the lifetime invariant).
    #[test]
    fn restore_session_row_preserves_write_once_stamps() {
        let dir = tempfile::tempdir().unwrap();
        let store = HostStore::open(dir.path().join("host.db"), EventBus::new()).unwrap();
        store.stamp_execution_mode("s-restore", "plan").unwrap();
        store.stamp_session_agent("s-restore", "coding").unwrap();
        assert_eq!(
            store.execution_mode_of("s-restore").as_deref(),
            Some("plan")
        );

        // A projcache row loss triggers the cold rebuild with only the
        // folded projection columns.
        store
            .restore_session_row("s-restore", Some("t"), Some("provider"), None, 3, 9)
            .unwrap();
        assert_eq!(
            store.execution_mode_of("s-restore").as_deref(),
            Some("plan"),
            "cold rebuild must not clear the write-once execution mode"
        );
        // The write-once contract still holds after the rebuild: a later
        // stamp for the same session is ignored.
        store.stamp_execution_mode("s-restore", "goal").unwrap();
        assert_eq!(
            store.execution_mode_of("s-restore").as_deref(),
            Some("plan"),
            "first execute still fixes the mode after a cold rebuild"
        );
        // Same write-once contract for the agent stamp: a second stamp is
        // ignored, and the rebuild must not have cleared it either (the
        // second stamp would then "succeed" and change the agent mid-life).
        store.stamp_session_agent("s-restore", "other").unwrap();
        let agent_fixed = store
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT agent FROM session_projcache WHERE session_id = 's-restore'",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        assert_eq!(
            agent_fixed.as_deref(),
            Some("coding"),
            "rebuild must keep the agent stamp and a later stamp must not change it"
        );
    }

    /// The synthetic sidebar buckets ("Ungrouped"/"Tasks") own their names: a
    /// directory with such a basename cannot be resolved into a workspace, and
    /// attach_by_path skips it silently instead of minting a colliding group.
    #[test]
    fn reserved_group_names_reject_creation_and_skip_auto_attach() {
        let dir = tempfile::tempdir().unwrap();
        let store = HostStore::open(dir.path().join("host.db"), EventBus::new()).unwrap();

        let reserved = dir.path().join("Ungrouped");
        std::fs::create_dir_all(&reserved).unwrap();
        assert!(store.resolve_workspace(reserved.to_str().unwrap()).is_err());
        // Re-resolving a pre-reservation workspace still works (regrouping).
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let (id, created) = store.resolve_workspace(proj.to_str().unwrap()).unwrap();
        assert!(created);
        assert!(store.rename_workspace(&id, "Tasks").is_err());
        store.rename_workspace(&id, "real name").unwrap();

        // attach_by_path over a reserved-name directory: skipped, no error,
        // and no colliding group ever appears.
        store
            .attach_by_path(reserved.to_str().unwrap(), "s-res")
            .unwrap();
        assert!(store
            .list_workspaces()
            .iter()
            .all(|w| w.title != "Ungrouped"));
    }

    #[test]
    fn attach_prepends_and_insert_before_reorders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p");
        std::fs::create_dir_all(&path).unwrap();
        let store = HostStore::open(dir.path().join("host.db"), EventBus::new()).unwrap();
        let (ws, _) = store.resolve_workspace(path.to_str().unwrap()).unwrap();

        store.attach_session(&ws, "s1").unwrap();
        store.attach_session(&ws, "s2").unwrap();
        store.attach_session(&ws, "s3").unwrap();
        let order = store.list_workspaces()[0].session_ids.clone();
        assert_eq!(
            order,
            vec!["s3".to_string(), "s2".to_string(), "s1".to_string()]
        );

        // Move s1 before s2.
        store.insert_before(&ws, "s1", Some("s2")).unwrap();
        let order = store.list_workspaces()[0].session_ids.clone();
        assert_eq!(
            order,
            vec!["s3".to_string(), "s1".to_string(), "s2".to_string()]
        );

        // Move s3 to the end.
        store.insert_before(&ws, "s3", None).unwrap();
        let order = store.list_workspaces()[0].session_ids.clone();
        assert_eq!(
            order,
            vec!["s1".to_string(), "s2".to_string(), "s3".to_string()]
        );
    }

    #[test]
    #[test]
    fn user_null_title_clears_the_override() {
        // Round-10 regression: rename(None) writes a user-sourced null title
        // — the projection must clear the pin so fallback titles apply again.
        let dir = tempfile::tempdir().unwrap();
        let store = HostStore::open(dir.path().join("host.db"), EventBus::new()).unwrap();
        upsert_title(&store.db.lock().unwrap(), "s1", Some("Pinned".into()), Some("user".into()))
            .unwrap();
        upsert_title(&store.db.lock().unwrap(), "s1", None, Some("user".into())).unwrap();
        let rows = store.list_sessions(false);
        let row = rows.iter().find(|r| r.session_id == "s1").unwrap();
        assert_eq!(row.title, None);
        assert_eq!(row.title_source, None);
        // A non-user null event must NOT clear a pin.
        upsert_title(&store.db.lock().unwrap(), "s1", Some("Pinned".into()), Some("user".into()))
            .unwrap();
        upsert_title(&store.db.lock().unwrap(), "s1", None, Some("fallback".into())).unwrap();
        let rows = store.list_sessions(false);
        let row = rows.iter().find(|r| r.session_id == "s1").unwrap();
        assert_eq!(row.title.as_deref(), Some("Pinned"));
    }

    #[test]
    fn fallback_title_never_overrides_provider_or_user_titles() {
        // Title-churn regression: every execute used to append a fallback title
        // reset to the task text; once the provider (LLM) title settles, later fallbacks
        // must not overwrite it (same for user pins) — or the sidebar title flips on every message.
        let dir = tempfile::tempdir().unwrap();
        let store = HostStore::open(dir.path().join("host.db"), EventBus::new()).unwrap();
        let upsert = |title: Option<&str>, source: &str| {
            upsert_title(
                &store.db.lock().unwrap(),
                "s1",
                title.map(String::from),
                Some(source.into()),
            )
            .unwrap();
        };
        let title_of = || {
            store
                .list_sessions(false)
                .iter()
                .find(|r| r.session_id == "s1")
                .unwrap()
                .title
                .clone()
        };

        upsert(Some("task A"), "fallback");
        assert_eq!(title_of().as_deref(), Some("task A"));
        upsert(Some("语义标题"), "provider");
        assert_eq!(title_of().as_deref(), Some("语义标题"));
        // Later sends' fallback no longer overwrites the provider title.
        upsert(Some("task B"), "fallback");
        assert_eq!(title_of().as_deref(), Some("语义标题"));
        // User pins rank highest and are immune to fallback.
        upsert(Some("手动命名"), "user");
        upsert(Some("task C"), "fallback");
        assert_eq!(title_of().as_deref(), Some("手动命名"));
    }

    #[test]
    fn delete_workspace_keeps_stale_session_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p");
        std::fs::create_dir_all(&path).unwrap();
        let store = HostStore::open(dir.path().join("host.db"), EventBus::new()).unwrap();
        let (ws, _) = store.resolve_workspace(path.to_str().unwrap()).unwrap();
        store.attach_session(&ws, "s1").unwrap();
        store.delete_workspace(&ws).unwrap();
        assert!(store.list_workspaces().is_empty());
        assert!(store.get("s1").is_some());
        // The stale workspace_id is load-bearing: NULL would make the orphaned
        // Coding session indistinguishable from a Chat conversation.
        assert!(store.get("s1").unwrap().workspace_id.is_some());
    }

    #[test]
    fn execution_mode_stamp_is_write_once() {
 // binding: the first execute stamps and locks — later stamps do not
        // change the value; sessions predating the binding read None.
        let dir = tempfile::tempdir().unwrap();
        let store = HostStore::open(dir.path().join("host.db"), EventBus::new()).unwrap();
        assert_eq!(store.execution_mode_of("s1"), None);
        store.stamp_execution_mode("s1", "plan").unwrap();
        assert_eq!(store.execution_mode_of("s1").as_deref(), Some("plan"));
        // A second stamp (e.g. a repeated execute) must not overwrite the lock.
        store.stamp_execution_mode("s1", "goal").unwrap();
        assert_eq!(store.execution_mode_of("s1").as_deref(), Some("plan"));
    }

    #[test]
    fn delete_workspace_removes_membership_rows() {
        // The members table declares ON DELETE CASCADE, which only fires with
        // `PRAGMA foreign_keys = ON`; without it rows leak per deleted group.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p");
        std::fs::create_dir_all(&path).unwrap();
        let db_path = dir.path().join("host.db");
        let store = HostStore::open(db_path.clone(), EventBus::new()).unwrap();
        let (ws, _) = store.resolve_workspace(path.to_str().unwrap()).unwrap();
        store.attach_session(&ws, "s1").unwrap();

        store.delete_workspace(&ws).unwrap();

        // Read the members table through a second connection (WAL allows it).
        let conn = Connection::open(&db_path).unwrap();
        let members: i64 = conn
            .query_row("SELECT COUNT(*) FROM workspace_members", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(members, 0, "membership rows must not outlive the group");
    }

    #[tokio::test]
    async fn attach_session_to_unknown_workspace_is_rejected() {
        // With referential integrity on, attaching to a missing group is an
        // error instead of silently writing an orphan member row.
        let dir = tempfile::tempdir().unwrap();
        let store = HostStore::open(dir.path().join("host.db"), EventBus::new()).unwrap();
        assert!(store.attach_session("no-such-workspace", "s1").is_err());
    }

    #[tokio::test]
    async fn projection_folds_title_archive_and_counts() {
        let bus = EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        let store = HostStore::open(dir.path().join("host.db"), bus.clone()).unwrap();
        let session = Session::new(SessionId("s1".into()), bus.clone());

        session
            .append_with(
                "session/title",
                title_event("hello task", "fallback"),
                AppendOptions::default(),
            )
            .unwrap();
        session
            .append_with(
                "assistant/message",
                assistant_event(),
                AppendOptions::default(),
            )
            .unwrap();
        session
            .append_with(
                "assistant/message",
                assistant_event(),
                AppendOptions::default(),
            )
            .unwrap();
        session
            .append_with(
                "session/archived",
                archived_event(true),
                AppendOptions::default(),
            )
            .unwrap();

        let row = store.get("s1").unwrap();
        assert_eq!(row.title.as_deref(), Some("hello task"));
        assert_eq!(row.title_source.as_deref(), Some("fallback"));
        assert_eq!(row.message_count, 2);
        assert!(row.archived_at.is_some());
    }

    #[tokio::test]
    async fn chunk_events_skip_projection_entirely() {
        // Streaming deltas are the log's overwhelming majority (41,686 in one
        // 12-minute turn); they must not touch the projection — the per-chunk
        // watermark advance ran one autocommit UPDATE per token inline on the
        // agent's streaming thread.
        let bus = EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        let store = HostStore::open(dir.path().join("host.db"), bus.clone()).unwrap();
        let session = Session::new(SessionId("s1".into()), bus.clone());

        session
            .append_with("assistant/chunk", chunk_event(), AppendOptions::default())
            .unwrap();
        assert!(
            store.get("s1").is_none(),
            "chunk events must not create or advance a projection row"
        );

        // The skip must not break counting: a later assistant message still
        // bumps (and advances the watermark past the skipped seq).
        session
            .append_with(
                "assistant/message",
                assistant_event(),
                AppendOptions::default(),
            )
            .unwrap();
        assert_eq!(store.get("s1").unwrap().message_count, 1);
    }

    #[tokio::test]
    async fn user_title_pin_wins_over_later_fallback() {
        let bus = EventBus::new();
        let dir = tempfile::tempdir().unwrap();
        let store = HostStore::open(dir.path().join("host.db"), bus.clone()).unwrap();
        let session = Session::new(SessionId("s1".into()), bus.clone());

        session
            .append_with(
                "session/title",
                title_event("auto", "fallback"),
                AppendOptions::default(),
            )
            .unwrap();
        session
            .append_with(
                "session/title",
                title_event("pinned", "user"),
                AppendOptions::default(),
            )
            .unwrap();
        session
            .append_with(
                "session/title",
                title_event("newer-auto", "provider"),
                AppendOptions::default(),
            )
            .unwrap();

        let row = store.get("s1").unwrap();
        assert_eq!(row.title.as_deref(), Some("pinned"));
        assert_eq!(row.title_source.as_deref(), Some("user"));
    }
}

// ── BUG-62 regression (round 5): cross-workspace move updates projcache ──
#[cfg(test)]
mod round5_tests {
    use super::*;
    use cordis_rslite::EventBus;

    /// `attach_session` and `detach_session` both maintain
    /// session_projcache.workspace_id; `insert_before` (the drag-and-drop
    /// move) must too, or list_sessions(Some(ws)) and the workspace member
    /// list disagree about where the session lives.
    #[test]
    fn insert_before_updates_the_projcache_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let store = HostStore::open(dir.path().join("host.db"), EventBus::new()).unwrap();
        let ws_a = dir.path().join("project-a");
        let ws_b = dir.path().join("project-b");
        std::fs::create_dir_all(&ws_a).unwrap();
        std::fs::create_dir_all(&ws_b).unwrap();
        let (a, _) = store
            .resolve_workspace(ws_a.to_str().unwrap())
            .unwrap();
        let (b, _) = store
            .resolve_workspace(ws_b.to_str().unwrap())
            .unwrap();

        store.attach_session(&a, "s-move").unwrap();
        store.insert_before(&b, "s-move", None).unwrap();
        assert_eq!(
            store.get("s-move").and_then(|row| row.workspace_id),
            Some(b.clone()),
            "moving a session across workspaces must move the projcache row too"
        );
    }
}
