//! Backend selection (WI-25): the kernel `SessionPersistence` seam is the
//! backend contract; SQLite (write-behind coordinator) and JSONL
//! (append-only files) are its two implementations. Construction-time
//! selection keeps the composition root in charge.

use std::path::PathBuf;
use std::sync::Arc;

use starriver_kernel::persistence::SessionPersistence;

use crate::jsonl::JsonlSessionPersistence;
use crate::SqliteSessionPersistence;

/// Which durable backend a store opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Single SQLite database file with a per-process write-behind
    /// coordinator (default; supports `prepare` for resume).
    Sqlite,
    /// Append-only JSONL files under a directory root; write-through fsync.
    Jsonl,
}

impl BackendKind {
    /// Parse a configuration string (`"sqlite"` / `"jsonl"`).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "sqlite" => Some(Self::Sqlite),
            "jsonl" => Some(Self::Jsonl),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Jsonl => "jsonl",
        }
    }
}

impl std::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Storage location for [`open_backend`]: a database file (SQLite) or a
/// directory root (JSONL).
#[derive(Debug, Clone)]
pub struct BackendConfig {
    pub kind: BackendKind,
    pub path: PathBuf,
}

/// Open the selected backend. The SQLite backend attaches to `bus` so live
/// `session/event` occurrences are write-behind persisted; JSONL ignores
/// the bus in this slice (appends are caller-driven, write-through).
pub fn open_backend(
    config: BackendConfig,
    bus: cordis_rslite::EventBus,
) -> Result<Arc<dyn SessionPersistence>, String> {
    match config.kind {
        BackendKind::Sqlite => Ok(Arc::new(SqliteSessionPersistence::open_with_bus(
            config.path,
            bus,
        )?)),
        BackendKind::Jsonl => Ok(Arc::new(JsonlSessionPersistence::open(config.path)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_parses_and_prints() {
        assert_eq!(BackendKind::parse("sqlite"), Some(BackendKind::Sqlite));
        assert_eq!(BackendKind::parse(" JSONL "), Some(BackendKind::Jsonl));
        assert_eq!(BackendKind::parse("rocksdb"), None);
        assert_eq!(BackendKind::Jsonl.to_string(), "jsonl");
    }

    #[tokio::test]
    async fn open_backend_returns_a_working_store_for_each_kind() {
        for kind in [BackendKind::Sqlite, BackendKind::Jsonl] {
            let dir = tempfile::tempdir().unwrap();
            let path = match kind {
                BackendKind::Sqlite => dir.path().join("sessions.db"),
                BackendKind::Jsonl => dir.path().join("root"),
            };
            let store =
                open_backend(BackendConfig { kind, path }, cordis_rslite::EventBus::new()).unwrap();
            let id = starriver_kernel::session::SessionId("s1".into());
            store
                .append(
                    id.clone(),
                    vec![starriver_kernel::session::SessionEvent {
                        seq: 0,
                        time: 1,
                        event_type: "turn/start".into(),
                        data: starriver_kernel::json::JsonValue::Null,
                        surface_op: None,
                        source_event_seqs: Vec::new(),
                    }],
                    None,
                )
                .await
                .unwrap();
            assert_eq!(store.list(None).await.unwrap().len(), 1);
        }
    }
}
