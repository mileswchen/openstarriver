//! Non-destructive read-only library inspection (WI-28): statistics
//! (sessions, logical/physical event counts, size, oldest/newest) plus the
//! canonical-schema drift report. Never initializes, mutates, or repairs.

use std::path::{Path, PathBuf};

use rusqlite::OpenFlags;

use crate::chunk_rows;
use crate::schema::{self, SchemaDrift};

/// Aggregate statistics for one session library.
#[derive(Debug, Clone)]
pub struct LibraryStats {
    pub path: PathBuf,
    /// Bytes of the database file plus `-wal`/`-shm` sidecars, when present.
    pub file_bytes: u64,
    pub store_id: Option<String>,
    pub application_id: i64,
    pub schema_version: i64,
    pub session_count: u64,
    /// Physical rows in the events table (packed rows count as one).
    pub physical_event_rows: u64,
    /// Logical events after expanding packed chunk rows.
    pub logical_event_count: u64,
    /// How many physical rows are packed chunk rows.
    pub packed_rows: u64,
    pub oldest_created_at: Option<u64>,
    pub newest_created_at: Option<u64>,
    /// Sessions whose event scan hit unreadable rows (tolerated; the rest
    /// of the report stays usable — a diagnostic tool must survive damage).
    pub corrupt_sessions: u64,
}

/// A full inspection report: stats plus schema drift findings.
#[derive(Debug, Clone)]
pub struct InspectReport {
    pub stats: LibraryStats,
    pub drift: Vec<SchemaDrift>,
}

impl InspectReport {
    /// Human-readable summary (also the `session-inspect` bin output).
    pub fn summary(&self) -> String {
        let stats = &self.stats;
        let mut out = String::new();
        out.push_str(&format!("path:            {}\n", stats.path.display()));
        out.push_str(&format!(
            "file bytes:      {} (db + wal/shm)\n",
            stats.file_bytes
        ));
        out.push_str(&format!(
            "store id:        {}\n",
            stats.store_id.as_deref().unwrap_or("<absent>")
        ));
        out.push_str(&format!(
            "application id:  0x{:08X}\n",
            stats.application_id
        ));
        out.push_str(&format!("schema version:  {}\n", stats.schema_version));
        out.push_str(&format!("sessions:        {}\n", stats.session_count));
        out.push_str(&format!(
            "events:          {} logical / {} physical rows ({} packed)\n",
            stats.logical_event_count, stats.physical_event_rows, stats.packed_rows
        ));
        out.push_str(&format!(
            "created_at:      oldest {} / newest {}\n",
            stats
                .oldest_created_at
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            stats
                .newest_created_at
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        ));
        if stats.corrupt_sessions > 0 {
            out.push_str(&format!(
                "corrupt sessions: {} (event scan hit unreadable rows)\n",
                stats.corrupt_sessions
            ));
        }
        if self.drift.is_empty() {
            out.push_str("schema drift:    none (canonical)\n");
        } else {
            out.push_str(&format!(
                "schema drift:    {} finding(s)\n",
                self.drift.len()
            ));
            for drift in &self.drift {
                out.push_str(&format!("  - {drift}\n"));
            }
        }
        out
    }
}

/// Inspect a SQLite session library read-only. Errors only when the file is
/// missing or not a database we can query at all.
pub fn inspect_library(path: &Path) -> Result<InspectReport, String> {
    if !path.exists() {
        return Err(format!("no session database at {}", path.display()));
    }
    let db = open_read_only_surviving_hot_wal(path)?;
    let stats = collect_stats(&db, path)?;
    let drift = schema::diff_schema(&db);
    Ok(InspectReport { stats, drift })
}

/// Open the library read-only, tolerating a hot `-wal` that needs recovery.
/// A plain READ_ONLY connection cannot run WAL recovery, so a crashed
/// process's leftover `-wal` makes every query fail with
/// SQLITE_READONLY_RECOVERY — exactly the crash state this diagnostic exists
/// for. Try immutable mode as the fallback (readers never mutate, matching
/// the read-only pin in the tests).
fn open_read_only_surviving_hot_wal(path: &Path) -> Result<rusqlite::Connection, String> {
    let plain = rusqlite::Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())?;
    // Probe with a trivial query: failure here is the recovery-needed case.
    if plain.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(())).is_ok() {
        return Ok(plain);
    }
    let uri = format!(
        "file://{}?immutable=1",
        path.to_string_lossy().replace('?', "%3f").replace('#', "%23")
    );
    rusqlite::Connection::open_with_flags(
        uri.as_str(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| e.to_string())
}

fn sidecar_bytes(path: &Path) -> u64 {
    let mut total = 0;
    for suffix in ["", "-wal", "-shm"] {
        let sidecar = if suffix.is_empty() {
            path.to_path_buf()
        } else {
            PathBuf::from(format!("{}{suffix}", path.display()))
        };
        if let Ok(meta) = std::fs::metadata(&sidecar) {
            total += meta.len();
        }
    }
    total
}

fn collect_stats(db: &rusqlite::Connection, path: &Path) -> Result<LibraryStats, String> {
    let application_id: i64 = db
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    let schema_version: i64 = db
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    let store_id: Option<String> = db
        .query_row("SELECT store_id FROM persistence_state", [], |row| {
            row.get(0)
        })
        .ok();
    let session_count: u64 = db
        .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    let physical_event_rows: u64 = db
        .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    let (oldest, newest): (Option<i64>, Option<i64>) = db
        .query_row(
            "SELECT MIN(created_at), MAX(created_at) FROM sessions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or((None, None));
    // Tolerant logical-event scan: packed chunk rows expand to their member
    // count; an unreadable row marks the session corrupt and keeps the rest
    // of the report usable.
    let mut logical_event_count = 0u64;
    let mut packed_rows = 0u64;
    let mut corrupt_sessions = 0u64;
    let session_ids: Vec<String> = {
        let mut stmt = db
            .prepare("SELECT id FROM sessions")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row.map_err(|e| e.to_string())?);
        }
        ids
    };
    for id in session_ids {
        let mut stmt = db
            .prepare("SELECT type, data FROM events WHERE session_id = ?1 ORDER BY seq")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([&id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?;
        let mut session_ok = true;
        for row in rows {
            let (event_type, data) = row.map_err(|e| e.to_string())?;
            // Compression-aware (WI-26): marker rows inflate before parsing.
            let decoded = crate::compression::decode_data_text(&data);
            let parse_text = decoded.as_deref().unwrap_or(&data);
            if chunk_rows::is_chunk_row_tag(&event_type) {
                packed_rows += 1;
                match starriver_kernel::json::JsonValue::from_json_str(parse_text)
                    .map_err(|e| e.to_string())
                    .and_then(|payload| count_chunk_members(&payload))
                {
                    Ok(members) => logical_event_count += members,
                    Err(_) => {
                        session_ok = false;
                        logical_event_count += 1;
                    }
                }
            } else {
                logical_event_count += 1;
                if decoded.is_err()
                    || starriver_kernel::json::JsonValue::from_json_str(parse_text).is_err()
                {
                    session_ok = false;
                }
            }
        }
        if !session_ok {
            corrupt_sessions += 1;
        }
    }
    Ok(LibraryStats {
        path: path.to_path_buf(),
        file_bytes: sidecar_bytes(path),
        store_id,
        application_id,
        schema_version,
        session_count,
        physical_event_rows,
        logical_event_count,
        packed_rows,
        oldest_created_at: oldest.map(|v| v.max(0) as u64),
        newest_created_at: newest.map(|v| v.max(0) as u64),
        corrupt_sessions,
    })
}

/// Member count of a packed row payload without full decoding: the
/// `texts`/`args` array length (matches `RunData::len`).
fn count_chunk_members(payload: &starriver_kernel::json::JsonValue) -> Result<u64, String> {
    for key in ["texts", "args"] {
        if let Some(array) = payload.get(key).and_then(|v| v.as_array()) {
            return Ok(array.len() as u64);
        }
    }
    Err("packed row payload carries no member array".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::json::JsonValue;
    use starriver_kernel::session::{SessionEvent, SessionHeader, SessionId};

    use crate::store;

    fn event(seq: u64, event_type: &str) -> SessionEvent {
        SessionEvent {
            seq,
            time: 1000 + seq,
            event_type: event_type.into(),
            data: JsonValue::Null,
            surface_op: None,
            source_event_seqs: Vec::new(),
        }
    }

    fn chunk_event(seq: u64, text: &str) -> SessionEvent {
        SessionEvent {
            seq,
            time: 1000 + seq,
            event_type: "assistant/chunk".into(),
            data: JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(1)),
                ("step".into(), JsonValue::UInt(1)),
                (
                    "chunk".into(),
                    JsonValue::Object(vec![
                        ("type".into(), JsonValue::String("text-delta".into())),
                        ("index".into(), JsonValue::UInt(0)),
                        ("text".into(), JsonValue::String(text.into())),
                    ]),
                ),
            ]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        }
    }

    fn seeded_library(path: &std::path::Path) {
        let mut db = rusqlite::Connection::open(path).unwrap();
        crate::schema::open_owned(&db, path).unwrap();
        let older = {
            let mut header = SessionHeader::new(SessionId("older".into()));
            header.created_at = 100;
            header
        };
        let newer = {
            let mut header = SessionHeader::new(SessionId("newer".into()));
            header.created_at = 900;
            header
        };
        store::append_batch(
            &mut db,
            &older,
            &[event(0, "turn/start"), event(1, "turn/end")],
            false,
        )
        .unwrap();
        // A 5-member packed run: one physical row, five logical events.
        store::append_batch(
            &mut db,
            &newer,
            &[
                event(0, "turn/start"),
                chunk_event(1, "a"),
                chunk_event(2, "b"),
                chunk_event(3, "c"),
                chunk_event(4, "d"),
                chunk_event(5, "e"),
            ],
            false,
        )
        .unwrap();
    }

    #[test]
    fn inspect_reports_stats_and_no_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        seeded_library(&path);
        let report = inspect_library(&path).unwrap();
        assert!(report.drift.is_empty(), "{:?}", report.drift);
        let stats = &report.stats;
        assert_eq!(stats.session_count, 2);
        assert_eq!(
            stats.physical_event_rows, 4,
            "2 (older) + turn/start + 1 packed"
        );
        assert_eq!(
            stats.logical_event_count, 8,
            "2 (older) + 1 + 5 chunk members"
        );
        assert_eq!(stats.packed_rows, 1);
        assert_eq!(stats.oldest_created_at, Some(100));
        assert_eq!(stats.newest_created_at, Some(900));
        assert_eq!(stats.corrupt_sessions, 0);
        assert!(stats.store_id.is_some());
        assert_eq!(stats.schema_version, crate::schema::SCHEMA_VERSION);
        let text = report.summary();
        assert!(text.contains("sessions:        2"));
        assert!(text.contains("schema drift:    none"));
    }

    #[test]
    fn inspect_tolerates_corruption_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        seeded_library(&path);
        {
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute(
                "UPDATE events SET data = '{torn' WHERE session_id = 'older'",
                [],
            )
            .unwrap();
        }
        let report = inspect_library(&path).unwrap();
        assert_eq!(report.stats.corrupt_sessions, 1);
        // The healthy session still counts.
        assert_eq!(report.stats.session_count, 2);
        assert_eq!(report.stats.packed_rows, 1);
    }

    #[test]
    fn inspect_detects_schema_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        seeded_library(&path);
        {
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute_batch(
                "ALTER TABLE sessions ADD COLUMN sneaky TEXT;
                 CREATE TABLE stowaway (x INTEGER);",
            )
            .unwrap();
        }
        let report = inspect_library(&path).unwrap();
        let kinds: Vec<&str> = report.drift.iter().map(|d| d.kind.as_str()).collect();
        assert!(kinds.contains(&"extra-column"), "{kinds:?}");
        assert!(kinds.contains(&"extra-table"), "{kinds:?}");
        assert!(report.summary().contains("finding(s)"));
    }

    #[test]
    fn inspect_is_read_only_and_rejects_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        seeded_library(&path);
        let before = std::fs::read(&path).unwrap();
        let mtime_before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let _ = inspect_library(&path).unwrap();
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after, "inspect must not modify the database file");
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            mtime_before
        );
        assert!(inspect_library(&dir.path().join("absent.db")).is_err());
    }
}
