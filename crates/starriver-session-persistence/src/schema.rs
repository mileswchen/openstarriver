//! SQLite schema ownership and opening flow.
//!
//! Opening a database is fail-closed: every security/durability pragma is
//! set and read back; an unversioned or foreign database is rejected instead
//! of guessed at. Mirrors `session-persistence-sqlite/schema.ts`.

use starriver_kernel::persistence::PersistenceError;

/// Custom application id magic: ASCII "STRR".
pub const APPLICATION_ID: i64 = 0x5354_5252;
/// Current schema version stamped into `PRAGMA user_version`.
pub const SCHEMA_VERSION: i64 = 1;

pub const SCHEMA_SQL: &str = r#"
CREATE TABLE persistence_state (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  store_id  TEXT NOT NULL
) STRICT;

CREATE TABLE sessions (
  id               TEXT PRIMARY KEY,
  version          INTEGER NOT NULL,
  created_at       INTEGER NOT NULL,
  cwd              TEXT,
  parent_session   TEXT,
  seed_length      INTEGER,
  origin           TEXT,
  delegation_depth INTEGER,
  agent_preset     TEXT,
  incarnation      TEXT NOT NULL,
  revision         INTEGER NOT NULL
) STRICT;

CREATE TABLE events (
  session_id        TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq               INTEGER NOT NULL,
  type              TEXT NOT NULL,
  time              INTEGER NOT NULL,
  data              TEXT NOT NULL,
  source_event_seqs TEXT,
  surface_op        TEXT,
  ignorable         INTEGER CHECK (ignorable IS NULL OR ignorable IN (0, 1)),
  PRIMARY KEY (session_id, seq)
) STRICT;
"#;

use rusqlite::Connection;

/// Open, validate ownership, initialize when clean, and configure durability.
/// Every step fails closed; callers must close the connection on error.
pub fn open_owned(db: &Connection, path: &std::path::Path) -> Result<(), String> {
    configure_security(db, path)?;
    configure_ownership_and_schema(db, path)?;
    configure_wal(db, path)?;
    configure_durability(db)?;
    validate_canonical_schema(db, path)?;
    Ok(())
}

fn configure_security(db: &Connection, path: &std::path::Path) -> Result<(), String> {
    db.execute_batch("PRAGMA trusted_schema = OFF;")
        .map_err(|e| e.to_string())?;
    let trusted: i64 = db
        .query_row("PRAGMA trusted_schema", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if trusted != 0 {
        return Err(format!(
            "database at {} retained trusted_schema={trusted}, expected 0",
            path.display()
        ));
    }
    db.execute_batch("PRAGMA mmap_size = 0;")
        .map_err(|e| e.to_string())?;
    if path != std::path::Path::new(":memory:") {
        let mmap: i64 = db
            .query_row("PRAGMA mmap_size", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        if mmap != 0 {
            return Err(format!(
                "database at {} retained mmap_size={mmap}, expected 0",
                path.display()
            ));
        }
    }
    Ok(())
}

fn configure_ownership_and_schema(db: &Connection, path: &std::path::Path) -> Result<(), String> {
    db.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|e| e.to_string())?;
    db.execute_batch("BEGIN IMMEDIATE;")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let on_disk: i64 = db
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        let application_id: i64 = db
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        let object_count: i64 = db
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type IN ('table','index')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if on_disk == 0 && (application_id != 0 || object_count > 0) {
            return Err(format!(
                "database at {} has an unversioned schema or application identity",
                path.display()
            ));
        }
        if on_disk != 0 && on_disk != SCHEMA_VERSION {
            return Err(format!(
                "database at {} has schema version {on_disk}, incompatible with this build ({SCHEMA_VERSION})",
                path.display()
            ));
        }
        if on_disk != 0 && application_id != APPLICATION_ID {
            return Err(format!(
                "database at {} has application id {application_id}, expected {APPLICATION_ID}",
                path.display()
            ));
        }
        if on_disk == 0 {
            initialize(db)?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => db.execute_batch("COMMIT;").map_err(|e| e.to_string()),
        Err(error) => {
            let _ = db.execute_batch("ROLLBACK;");
            Err(error)
        }
    }
}

fn initialize(db: &Connection) -> Result<(), String> {
    db.execute_batch(SCHEMA_SQL).map_err(|e| e.to_string())?;
    let store_id = uuid::Uuid::new_v4().to_string();
    db.execute(
        "INSERT INTO persistence_state (singleton, store_id) VALUES (1, ?1)",
        [&store_id],
    )
    .map_err(|e| e.to_string())?;
    db.execute_batch(&format!("PRAGMA application_id = {APPLICATION_ID};"))
        .map_err(|e| e.to_string())?;
    db.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn configure_wal(db: &Connection, path: &std::path::Path) -> Result<(), String> {
    db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
        .map_err(|e| e.to_string())?;
    let mode: String = db
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    let expected = if path == std::path::Path::new(":memory:") {
        "memory"
    } else {
        "wal"
    };
    if mode.to_ascii_lowercase() != expected {
        return Err(format!(
            "database at {} selected journal mode {mode}, expected {expected}",
            path.display()
        ));
    }
    Ok(())
}

fn configure_durability(db: &Connection) -> Result<(), String> {
    db.execute_batch("PRAGMA synchronous = FULL;")
        .map_err(|e| e.to_string())?;
    let synchronous: i64 = db
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .map_err(|e| e.to_string())?;
    if synchronous != 2 {
        return Err(format!(
            "retained synchronous={synchronous}, expected FULL (2)"
        ));
    }
    Ok(())
}

/// Recheck ownership inside a caller's mutation transaction. Any external
/// writer that changed app id, schema, or version rejects this transaction.
pub fn validate_mutation_ownership(tx: &rusqlite::Transaction<'_>) -> Result<(), PersistenceError> {
    let application_id: i64 = tx
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    if application_id != APPLICATION_ID {
        return Err(PersistenceError::Corruption(format!(
            "session database application id changed before mutation (expected {APPLICATION_ID}, got {application_id})"
        )));
    }
    let version: i64 = tx
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    if version != SCHEMA_VERSION {
        return Err(PersistenceError::Corruption(format!(
            "session database schema changed before mutation (expected {SCHEMA_VERSION}, got {version})"
        )));
    }
    validate_canonical_schema(tx, std::path::Path::new("<mutation>"))
        .map_err(PersistenceError::Corruption)
}

// ---------------------------------------------------------------------------
// Canonical schema full comparison (WI-28)
//
// The canonical definition is `SCHEMA_SQL` itself: a private in-memory
// reference database is built from it and introspected, then compared
// against the target's PRAGMA-reported reality. This "reference build"
// approach keeps the comparison honest by construction — the DDL and the
// compared signature can never drift apart. CHECK/FK constraints are not
// compared (PRAGMA has no stable introspection for them); SQLite-internal
// `sqlite_%` objects (autoindexes) are exempt.

/// One column's PRAGMA `table_info` signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSignature {
    pub name: String,
    pub decl_type: String,
    pub notnull: bool,
    pub default: Option<String>,
    /// Primary-key ordinal (0 = not part of the PK).
    pub pk: u8,
}

/// One table's signature: columns plus explicitly-created indexes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSignature {
    pub name: String,
    pub columns: Vec<ColumnSignature>,
    pub indexes: Vec<String>,
}

/// Drift classification between the canonical schema and a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaDriftKind {
    MissingTable,
    ExtraTable,
    MissingColumn,
    ExtraColumn,
    ColumnSignatureMismatch,
    ExtraIndex,
}

impl SchemaDriftKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingTable => "missing-table",
            Self::ExtraTable => "extra-table",
            Self::MissingColumn => "missing-column",
            Self::ExtraColumn => "extra-column",
            Self::ColumnSignatureMismatch => "column-signature-mismatch",
            Self::ExtraIndex => "extra-index",
        }
    }
}

/// One concrete drift finding with a human-readable detail.
#[derive(Debug, Clone)]
pub struct SchemaDrift {
    pub kind: SchemaDriftKind,
    pub table: String,
    pub column: Option<String>,
    pub detail: String,
}

impl std::fmt::Display for SchemaDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} on `{}`: {}",
            self.kind.as_str(),
            self.table,
            self.detail
        )
    }
}

/// Build the canonical table signatures from a fresh in-memory reference
/// database created out of [`SCHEMA_SQL`].
pub fn canonical_schema() -> Result<Vec<TableSignature>, String> {
    let reference = Connection::open_in_memory().map_err(|e| e.to_string())?;
    reference
        .execute_batch(SCHEMA_SQL)
        .map_err(|e| e.to_string())?;
    introspect(&reference)
}

/// Introspect a database's user tables (full column signatures) and
/// explicitly-created indexes. SQLite-internal objects are excluded.
pub fn introspect(db: &Connection) -> Result<Vec<TableSignature>, String> {
    let mut names: Vec<String> = Vec::new();
    {
        let mut stmt = db
            .prepare(
                "SELECT name FROM sqlite_schema
                 WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        for row in rows {
            names.push(row.map_err(|e| e.to_string())?);
        }
    }
    let mut tables = Vec::new();
    for name in names {
        let quoted = format!("PRAGMA table_info(\"{}\")", name.replace('"', "\"\""));
        let mut columns = Vec::new();
        {
            let mut stmt = db.prepare(&quoted).map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(ColumnSignature {
                        name: row.get(1)?,
                        decl_type: row.get(2)?,
                        notnull: row.get(3)?,
                        default: row.get(4)?,
                        pk: row.get(5)?,
                    })
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                columns.push(row.map_err(|e| e.to_string())?);
            }
        }
        let mut indexes = Vec::new();
        {
            let mut stmt = db
                .prepare(
                    "SELECT name FROM sqlite_schema
                     WHERE type='index' AND name NOT LIKE 'sqlite_%' AND tbl_name = ?1
                     ORDER BY name",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([&name], |row| row.get::<_, String>(0))
                .map_err(|e| e.to_string())?;
            for row in rows {
                indexes.push(row.map_err(|e| e.to_string())?);
            }
        }
        tables.push(TableSignature {
            name,
            columns,
            indexes,
        });
    }
    Ok(tables)
}

/// Compare a database against the canonical schema; every difference is one
/// drift entry (never an early return — callers want the full report).
pub fn diff_schema(db: &Connection) -> Vec<SchemaDrift> {
    let (canonical, actual) = match (canonical_schema(), introspect(db)) {
        (Ok(canonical), Ok(actual)) => (canonical, actual),
        (Err(error), _) | (_, Err(error)) => {
            return vec![SchemaDrift {
                kind: SchemaDriftKind::MissingTable,
                table: "<introspection>".into(),
                column: None,
                detail: format!("schema introspection failed: {error}"),
            }];
        }
    };
    let mut drifts = Vec::new();
    fn find<'a>(tables: &'a [TableSignature], name: &str) -> Option<&'a TableSignature> {
        tables.iter().find(|table| table.name == name)
    }
    for want in &canonical {
        let Some(have) = find(&actual, &want.name) else {
            drifts.push(SchemaDrift {
                kind: SchemaDriftKind::MissingTable,
                table: want.name.clone(),
                column: None,
                detail: "table expected by this build is absent".into(),
            });
            continue;
        };
        for column in &want.columns {
            match have.columns.iter().find(|c| c.name == column.name) {
                None => drifts.push(SchemaDrift {
                    kind: SchemaDriftKind::MissingColumn,
                    table: want.name.clone(),
                    column: Some(column.name.clone()),
                    detail: format!("column `{}` ({}) is absent", column.name, column.decl_type),
                }),
                Some(found) if found != column => drifts.push(SchemaDrift {
                    kind: SchemaDriftKind::ColumnSignatureMismatch,
                    table: want.name.clone(),
                    column: Some(column.name.clone()),
                    detail: format!("expected {:?}, found {:?}", column, found),
                }),
                Some(_) => {}
            }
        }
        for column in &have.columns {
            if !want.columns.iter().any(|c| c.name == column.name) {
                drifts.push(SchemaDrift {
                    kind: SchemaDriftKind::ExtraColumn,
                    table: want.name.clone(),
                    column: Some(column.name.clone()),
                    detail: format!("column `{}` is not in the canonical schema", column.name),
                });
            }
        }
        for index in &have.indexes {
            if !want.indexes.contains(index) {
                drifts.push(SchemaDrift {
                    kind: SchemaDriftKind::ExtraIndex,
                    table: want.name.clone(),
                    column: None,
                    detail: format!("explicit index `{}` is not canonical", index),
                });
            }
        }
    }
    for table in &actual {
        if find(&canonical, &table.name).is_none() {
            drifts.push(SchemaDrift {
                kind: SchemaDriftKind::ExtraTable,
                table: table.name.clone(),
                column: None,
                detail: "table is not in the canonical schema".into(),
            });
        }
    }
    drifts
}

/// Fail-closed full-schema validation: any drift rejects with a report of
/// every finding (WI-28 upgrade of the required-objects subset check).
pub fn validate_canonical_schema(db: &Connection, path: &std::path::Path) -> Result<(), String> {
    let drifts = diff_schema(db);
    if drifts.is_empty() {
        return Ok(());
    }
    let listing = drifts
        .iter()
        .map(|drift| format!("  - {drift}"))
        .collect::<Vec<_>>()
        .join("\n");
    Err(format!(
        "database at {} drifted from the canonical schema ({} finding(s)):\n{listing}",
        path.display(),
        drifts.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open(path: &std::path::Path) -> Connection {
        Connection::open(path).expect("open")
    }

    #[test]
    fn fresh_database_initializes_owned_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let db = open(&path);
        super::open_owned(&db, &path).unwrap();
        let app_id: i64 = db
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .unwrap();
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(app_id, APPLICATION_ID);
        assert_eq!(version, SCHEMA_VERSION);
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type IN ('table','index')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(count >= 3, "expected at least the three schema tables");
    }

    #[test]
    fn unversioned_database_with_objects_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foreign.db");
        let db = open(&path);
        db.execute_batch("CREATE TABLE junk (x INTEGER);").unwrap();
        assert!(super::open_owned(&db, &path).is_err());
    }

    #[test]
    fn wrong_application_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let db = open(&path);
        super::open_owned(&db, &path).unwrap();
        db.execute_batch("PRAGMA application_id = 12345;").unwrap();
        assert!(super::open_owned(&db, &path).is_err());
    }

    #[test]
    fn wrong_schema_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let db = open(&path);
        super::open_owned(&db, &path).unwrap();
        db.execute_batch("PRAGMA user_version = 99;").unwrap();
        assert!(super::open_owned(&db, &path).is_err());
    }

    #[test]
    fn missing_required_table_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let db = open(&path);
        super::open_owned(&db, &path).unwrap();
        db.execute_batch("DROP TABLE events;").unwrap();
        assert!(super::open_owned(&db, &path).is_err());
    }

    #[test]
    fn reopening_owned_database_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let db = open(&path);
        super::open_owned(&db, &path).unwrap();
        super::open_owned(&db, &path).unwrap();
        let store_id: String = db
            .query_row("SELECT store_id FROM persistence_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(!store_id.is_empty());
    }

    #[test]
    fn trusted_schema_stays_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let db = open(&path);
        super::open_owned(&db, &path).unwrap();
        let trusted: i64 = db
            .query_row("PRAGMA trusted_schema", [], |row| row.get(0))
            .unwrap();
        assert_eq!(trusted, 0);
    }

    #[test]
    fn fresh_database_matches_canonical_exactly() {
        // The canonical definition is SCHEMA_SQL built into a reference
        // database; a freshly opened library must show zero drift.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let db = open(&path);
        super::open_owned(&db, &path).unwrap();
        assert!(super::diff_schema(&db).is_empty());
        assert_eq!(super::canonical_schema().unwrap().len(), 3);
    }

    #[test]
    fn canonical_diff_classifies_every_drift_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let db = open(&path);
        super::open_owned(&db, &path).unwrap();
        // Missing column + signature mismatch + extra column.
        db.execute_batch(
            "CREATE TABLE sessions_backup AS SELECT * FROM sessions;
             DROP TABLE events;
             CREATE TABLE events (
               session_id        TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
               seq               INTEGER NOT NULL,
               type              REAL NOT NULL,
               time              INTEGER NOT NULL,
               data              TEXT NOT NULL,
               source_event_seqs TEXT,
               surface_op        TEXT,
               ignorable         INTEGER,
               intruder          TEXT,
               PRIMARY KEY (session_id, seq)
             ) STRICT;
             INSERT INTO sessions_backup SELECT * FROM sessions;
             DROP TABLE sessions_backup;",
        )
        .unwrap();
        let drifts = super::diff_schema(&db);
        let kinds: Vec<&str> = drifts.iter().map(|d| d.kind.as_str()).collect();
        // events.type changed TEXT -> REAL (signature mismatch).
        assert!(kinds.contains(&"column-signature-mismatch"), "{kinds:?}");
        // events.intruder appeared (extra column).
        assert!(kinds.contains(&"extra-column"), "{kinds:?}");
        // Extra explicit index.
        db.execute_batch("CREATE INDEX drift_probe ON sessions (cwd);")
            .unwrap();
        let drifts = super::diff_schema(&db);
        let kinds: Vec<&str> = drifts.iter().map(|d| d.kind.as_str()).collect();
        assert!(kinds.contains(&"extra-index"), "{kinds:?}");
        // Extra table.
        db.execute_batch("CREATE TABLE drift_table (x INTEGER);")
            .unwrap();
        let drifts = super::diff_schema(&db);
        let kinds: Vec<&str> = drifts.iter().map(|d| d.kind.as_str()).collect();
        assert!(kinds.contains(&"extra-table"), "{kinds:?}");
        // Missing table.
        let dir2 = tempfile::tempdir().unwrap();
        let path2 = dir2.path().join("s2.db");
        let db2 = open(&path2);
        super::open_owned(&db2, &path2).unwrap();
        db2.execute_batch("DROP TABLE events;").unwrap();
        let kinds: Vec<&str> = super::diff_schema(&db2)
            .iter()
            .map(|d| d.kind.as_str())
            .collect();
        assert!(kinds.contains(&"missing-table"), "{kinds:?}");
        // Missing column.
        let dir3 = tempfile::tempdir().unwrap();
        let path3 = dir3.path().join("s3.db");
        let db3 = open(&path3);
        super::open_owned(&db3, &path3).unwrap();
        // SQLite cannot DROP COLUMN-originate from older versions, but the
        // bundled build supports it; reproduce a missing column by
        // rebuilding events without `ignorable`.
        db3.execute_batch(
            "CREATE TABLE events_lite AS SELECT session_id, seq, type, time, data,
                    source_event_seqs, surface_op FROM events;
             DROP TABLE events;
             ALTER TABLE events_lite RENAME TO events;",
        )
        .unwrap();
        let kinds: Vec<&str> = super::diff_schema(&db3)
            .iter()
            .map(|d| d.kind.as_str())
            .collect();
        assert!(kinds.contains(&"missing-column"), "{kinds:?}");
    }

    #[test]
    fn open_owned_rejects_drifted_database_with_findings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        let db = open(&path);
        super::open_owned(&db, &path).unwrap();
        db.execute_batch("CREATE TABLE drift_table (x INTEGER);")
            .unwrap();
        let error = super::open_owned(&db, &path).unwrap_err();
        assert!(
            error.contains("drifted from the canonical schema"),
            "{error}"
        );
        assert!(error.contains("extra-table"), "{error}");
    }
}
