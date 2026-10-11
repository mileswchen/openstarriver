//! SQLite backend primitives: durable row reads/writes with strict
//! ownership and contiguity checks. The coordinator owns batching and retry.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use starriver_kernel::persistence::PersistenceError;
use starriver_kernel::session::{SessionEvent, SessionHeader, SessionId};

use crate::chunk_rows;
use crate::compression;
use crate::schema;

/// A raw stored session row (header + events + revision).
#[derive(Debug, Clone)]
pub struct StoredSession {
    pub header: SessionHeader,
    pub events: Vec<SessionEvent>,
    /// First seq of a torn physical tail (corrupt last row), if any. The
    /// caller repairs by truncating everything at or after this seq.
    pub torn_from: Option<u64>,
    /// Source-qualified revision: `store:incarnation:revision`.
    pub revision: String,
}

pub fn load_stored(
    db: &Connection,
    store_id: &str,
    id: &SessionId,
) -> Result<Option<StoredSession>, PersistenceError> {
    let row = db
        .query_row(
            "SELECT version, created_at, cwd, parent_session, seed_length, origin,
                    delegation_depth, agent_preset, incarnation, revision
             FROM sessions WHERE id = ?1",
            [&id.0],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, i64>(9)?,
                ))
            },
        )
        .optional()
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    let Some((
        version,
        created_at,
        cwd,
        parent,
        seed_length,
        origin,
        delegation,
        preset,
        incarnation,
        revision,
    )) = row
    else {
        return Ok(None);
    };
    // The kernel contract: a persistence backend REJECTS any other
    // version on load — there is no migration. The old code accepted (and
    // u8-truncated) arbitrary version integers.
    if (version as u64) != starriver_kernel::session::SESSION_FORMAT_VERSION as u64 {
        return Err(PersistenceError::FormatUnsupported(format!(
            "session {} stored with format version {version}; this build only understands {}",
            id.0,
            starriver_kernel::session::SESSION_FORMAT_VERSION
        )));
    }
    let (events, torn_from) = select_events(db, id)?;
    let header = SessionHeader {
        version: version as u8,
        id: id.clone(),
        created_at: created_at as u64,
        cwd,
        parent_session: parent.map(SessionId),
        seed_length: seed_length.map(|v| v as u64),
        origin: origin
            .filter(|o| o == "subagent")
            .map(|_| starriver_kernel::session::SessionOrigin::Subagent),
        delegation_depth: delegation.map(|v| v as u32),
        agent_preset: preset,
    };
    Ok(Some(StoredSession {
        header,
        events,
        torn_from,
        revision: format!("{store_id}:incarnation:{incarnation}:revision:{revision}"),
    }))
}

/// Select every event of a session. Returns the valid events plus the seq of
/// the first physically corrupt row, when that row is the log tail. A corrupt
/// row in the committed prefix (not the tail) rejects as corruption; a torn
/// tail is the caller's repair signal. Packed chunk rows expand to their
/// exact member events; the logical stream must stay contiguous — an overlap
/// between a packed span and its neighbors is corruption.
fn select_events(
    db: &Connection,
    id: &SessionId,
) -> Result<(Vec<SessionEvent>, Option<u64>), PersistenceError> {
    let mut stmt = db
        .prepare(
            "SELECT seq, type, time, data, source_event_seqs, surface_op,
                    (SELECT MAX(seq) FROM events WHERE session_id = ?1) AS max_seq
             FROM events WHERE session_id = ?1 ORDER BY seq",
        )
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    let rows = stmt
        .query_map([&id.0], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    let mut events = Vec::new();
    let torn_tail_or_corruption =
        |events: Vec<SessionEvent>,
         seq: i64,
         max_seq: i64,
         why: String|
         -> Result<(Vec<SessionEvent>, Option<u64>), PersistenceError> {
            if seq == max_seq {
                // Torn tail: stop reading; the caller repairs by truncation.
                return Ok((events, Some(seq as u64)));
            }
            Err(PersistenceError::Corruption(format!(
                "event {seq} of session {} is corrupt: {why}",
                id.0
            )))
        };
    for row in rows {
        let (seq, event_type, time, data, source_event_seqs, surface_op, max_seq) =
            row.map_err(|e| PersistenceError::Io(e.to_string()))?;
        // Compressed rows (zstd1: marker, WI-26) inflate before parsing; a
        // malformed frame is corruption like any unreadable payload.
        let data_text = match compression::decode_data_text(&data) {
            Ok(text) => text,
            Err(why) => {
                return torn_tail_or_corruption(events, seq, max_seq, why);
            }
        };
        let data = match starriver_kernel::json::JsonValue::from_json_str(&data_text) {
            Ok(data) => data,
            Err(error) => {
                return torn_tail_or_corruption(
                    events,
                    seq,
                    max_seq,
                    format!("invalid data: {error}"),
                );
            }
        };
        let expanded = if chunk_rows::is_chunk_row_tag(&event_type) {
            if surface_op.is_some() || source_event_seqs.is_some() {
                return torn_tail_or_corruption(
                    events,
                    seq,
                    max_seq,
                    "packed chunk row must not carry surface metadata".into(),
                );
            }
            match chunk_rows::decode_chunk_row(&event_type, seq as u64, time as u64, &data) {
                Ok(members) => members,
                Err(why) => {
                    return torn_tail_or_corruption(events, seq, max_seq, why);
                }
            }
        } else {
            vec![SessionEvent {
                seq: seq as u64,
                time: time as u64,
                event_type,
                data,
                surface_op: surface_op
                    .as_deref()
                    .map(serde_json::from_str::<SurfaceOpWire>)
                    .transpose()
                    .map_err(|e| PersistenceError::Corruption(e.to_string()))?
                    .map(SurfaceOpWire::into_kernel),
                source_event_seqs: source_event_seqs
                    .map(|s| serde_json::from_str::<Vec<u64>>(&s))
                    .transpose()
                    .map_err(|e| PersistenceError::Corruption(e.to_string()))?
                    .unwrap_or_default(),
            }]
        };
        for event in expanded {
            if event.seq != events.len() as u64 {
                return Err(PersistenceError::Corruption(format!(
                    "event {} of session {} breaks seq contiguity (expected {})",
                    event.seq,
                    id.0,
                    events.len()
                )));
            }
            events.push(event);
        }
    }
    Ok((events, None))
}

/// Durable append of a contiguous batch, lazily materializing the session
/// row on first write. Runs inside a single immediate transaction.
pub fn append_batch(
    db: &mut Connection,
    header: &SessionHeader,
    events: &[SessionEvent],
    materialized: bool,
) -> Result<(), PersistenceError> {
    if events.is_empty() {
        return Ok(());
    }
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    let result = (|| {
        schema::validate_mutation_ownership(&tx)?;
        let stored_next = next_seq(&tx, &header.id)?;
        for (index, event) in events.iter().enumerate() {
            if event.seq != stored_next + index as u64 {
                return Err(PersistenceError::Discontiguous {
                    id: header.id.clone(),
                    expected: stored_next + index as u64,
                    got: event.seq,
                    index,
                });
            }
        }
        if !materialized {
            write_session_row(&tx, header)?;
        }
        let mut insert = tx
            .prepare(
                "INSERT INTO events
                   (session_id, seq, type, time, data, source_event_seqs, surface_op)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .map_err(|e| PersistenceError::Io(e.to_string()))?;
        for record in chunk_rows::pack_chunk_runs(events) {
            match record {
                chunk_rows::StorageRecord::Event(event) => {
                    let wire = SurfaceOpWire::from_kernel(event.surface_op);
                    let data_text = event
                        .data
                        .to_json_string()
                        .map_err(|e| PersistenceError::Corruption(e.to_string()))?;
                    insert
                        .execute(rusqlite::params![
                            &header.id.0,
                            event.seq as i64,
                            &event.event_type,
                            event.time as i64,
                            compression::encode_data_text(&data_text)
                                .map_err(PersistenceError::Corruption)?,
                            if event.source_event_seqs.is_empty() {
                                None
                            } else {
                                Some(serde_json::to_string(&event.source_event_seqs).unwrap())
                            },
                            wire.map(|w| serde_json::to_string(&w).unwrap()),
                        ])
                        .map_err(|e| PersistenceError::Io(e.to_string()))?;
                }
                chunk_rows::StorageRecord::ChunkRow {
                    seq0,
                    time0,
                    kind,
                    data,
                } => {
                    // One physical row covers the whole run: seq/time key the
                    // FIRST member; the tag rides the type column (bare,
                    // slash-less — disjoint from the event taxonomy).
                    let payload = chunk_rows::chunk_row_data_json(&data)
                        .map_err(PersistenceError::Corruption)?;
                    insert
                        .execute(rusqlite::params![
                            &header.id.0,
                            seq0 as i64,
                            kind.row_tag(),
                            time0 as i64,
                            compression::encode_data_text(&payload)
                                .map_err(PersistenceError::Corruption)?,
                            None::<String>,
                            None::<String>,
                        ])
                        .map_err(|e| PersistenceError::Io(e.to_string()))?;
                }
            }
        }
        drop(insert);
        increment_revision(&tx, &header.id)?;
        Ok(())
    })();
    match result {
        Ok(()) => tx.commit().map_err(|e| PersistenceError::Io(e.to_string())),
        Err(error) => {
            let _ = tx.rollback();
            Err(error)
        }
    }
}

/// Next free seq for `id`: one past the LOGICAL last event. A packed chunk
/// row keys its first member's seq in the `seq` column, so a packed tail
/// contributes its member count, not one.
fn next_seq(db: &Connection, id: &SessionId) -> Result<u64, PersistenceError> {
    let last: Option<(i64, String, i64, String)> = db
        .query_row(
            "SELECT seq, type, time, data FROM events
             WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
            [&id.0],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    let Some((seq, event_type, time, data)) = last else {
        return Ok(0);
    };
    if chunk_rows::is_chunk_row_tag(&event_type) {
        let data_text = compression::decode_data_text(&data).map_err(|e| {
            PersistenceError::Corruption(format!("session {} packed tail is unreadable: {e}", id.0))
        })?;
        let parsed = starriver_kernel::json::JsonValue::from_json_str(&data_text).map_err(|e| {
            PersistenceError::Corruption(format!(
                "session {} packed tail has invalid data: {e}",
                id.0
            ))
        })?;
        let members = chunk_rows::decode_chunk_row(&event_type, seq as u64, time as u64, &parsed)
            .map_err(PersistenceError::Corruption)?;
        return Ok(seq as u64 + members.len() as u64);
    }
    Ok(seq as u64 + 1)
}

fn write_session_row(
    tx: &rusqlite::Transaction<'_>,
    header: &SessionHeader,
) -> Result<(), PersistenceError> {
    tx.execute(
        "INSERT INTO sessions
           (id, version, created_at, cwd, parent_session, seed_length, origin,
            delegation_depth, agent_preset, incarnation, revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0)",
        rusqlite::params![
            &header.id.0,
            header.version as i64,
            header.created_at as i64,
            &header.cwd,
            header.parent_session.as_ref().map(|id| &id.0),
            header.seed_length.map(|v| v as i64),
            header.origin.map(|_| "subagent"),
            header.delegation_depth.map(|v| v as i64),
            &header.agent_preset,
            uuid::Uuid::new_v4().to_string(),
        ],
    )
    .map_err(|e| PersistenceError::Io(e.to_string()))?;
    Ok(())
}

fn increment_revision(
    tx: &rusqlite::Transaction<'_>,
    id: &SessionId,
) -> Result<(), PersistenceError> {
    let updated = tx
        .execute(
            "UPDATE sessions SET revision = revision + 1 WHERE id = ?1",
            [&id.0],
        )
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    if updated != 1 {
        return Err(PersistenceError::Corruption(format!(
            "session {} metadata row is missing",
            id.0
        )));
    }
    Ok(())
}

pub fn read_stored_revision(
    db: &Connection,
    store_id: &str,
    id: &SessionId,
) -> Result<Option<String>, PersistenceError> {
    let row = db
        .query_row(
            "SELECT incarnation, revision FROM sessions WHERE id = ?1",
            [&id.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    Ok(row.map(|(incarnation, revision)| {
        format!("{store_id}:incarnation:{incarnation}:revision:{revision}")
    }))
}

pub fn list_sessions(db: &Connection) -> Result<Vec<SessionHeader>, PersistenceError> {
    let mut stmt = db
        .prepare(
            "SELECT id, version, created_at, cwd, parent_session, seed_length, origin,
                    delegation_depth, agent_preset
             FROM sessions ORDER BY created_at, id",
        )
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    let mut headers = Vec::new();
    for row in rows {
        let (id, version, created_at, cwd, parent, seed_length, origin, delegation, preset) =
            row.map_err(|e| PersistenceError::Io(e.to_string()))?;
        headers.push(SessionHeader {
            version: version as u8,
            id: SessionId(id),
            created_at: created_at as u64,
            cwd,
            parent_session: parent.map(SessionId),
            seed_length: seed_length.map(|v| v as u64),
            origin: origin
                .filter(|o| o == "subagent")
                .map(|_| starriver_kernel::session::SessionOrigin::Subagent),
            delegation_depth: delegation.map(|v| v as u32),
            agent_preset: preset,
        });
    }
    Ok(headers)
}

pub fn remove_session(db: &Connection, id: &SessionId) -> Result<(), PersistenceError> {
    db.execute("DELETE FROM sessions WHERE id = ?1", [&id.0])
        .map(|_| ())
        .map_err(|e| PersistenceError::Io(e.to_string()))
}

/// Truncate a torn physical tail: delete every event at or after `from_seq`.
/// Runs inside the caller's mutation transaction.
pub fn truncate_torn_tail(
    tx: &rusqlite::Transaction<'_>,
    id: &SessionId,
    from_seq: u64,
) -> Result<(), PersistenceError> {
    schema::validate_mutation_ownership(tx)?;
    tx.execute(
        "DELETE FROM events WHERE session_id = ?1 AND seq >= ?2",
        rusqlite::params![&id.0, from_seq as i64],
    )
    .map(|_| ())
    .map_err(|e| PersistenceError::Io(e.to_string()))
}

/// Wire form of `SurfaceOp` for the `surface_op` TEXT column.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SurfaceOpWire {
    op: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end: Option<u64>,
}

impl SurfaceOpWire {
    pub fn from_kernel(op: Option<starriver_kernel::session::SurfaceOp>) -> Option<Self> {
        op.map(|op| match op {
            starriver_kernel::session::SurfaceOp::Append => Self {
                op: "append".into(),
                start: None,
                end: None,
            },
            starriver_kernel::session::SurfaceOp::Replace { start, end } => Self {
                op: "replace".into(),
                start: Some(start),
                end: Some(end),
            },
        })
    }

    pub fn into_kernel(self) -> starriver_kernel::session::SurfaceOp {
        match self.op.as_str() {
            "replace" => starriver_kernel::session::SurfaceOp::Replace {
                start: self.start.unwrap_or(0),
                end: self.end.unwrap_or(0),
            },
            _ => starriver_kernel::session::SurfaceOp::Append,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starriver_kernel::json::JsonValue;
    use starriver_kernel::session::SurfaceOp;

    fn header(id: &str) -> SessionHeader {
        let mut header = SessionHeader::new(SessionId(id.into()));
        header.cwd = Some("/tmp".into());
        header
    }

    fn event(seq: u64, event_type: &str) -> SessionEvent {
        SessionEvent {
            seq,
            time: 1,
            event_type: event_type.into(),
            data: JsonValue::Null,
            surface_op: None,
            source_event_seqs: Vec::new(),
        }
    }

    fn open(path: &std::path::Path) -> Connection {
        let db = Connection::open(path).expect("open");
        schema::open_owned(&db, path).expect("owned");
        db
    }

    fn store_id(db: &Connection) -> String {
        db.query_row("SELECT store_id FROM persistence_state", [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    #[test]
    fn lazy_materialization_appears_only_after_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut db = open(&path);
        let id = SessionId("s1".into());
        let h = header("s1");
        assert!(load_stored(&db, &store_id(&db), &id).unwrap().is_none());
        assert!(list_sessions(&db).unwrap().is_empty());
        append_batch(&mut db, &h, &[event(0, "turn/start")], false).unwrap();
        let stored = load_stored(&db, &store_id(&db), &id).unwrap().unwrap();
        assert_eq!(stored.events.len(), 1);
        assert_eq!(list_sessions(&db).unwrap().len(), 1);
    }

    #[test]
    fn append_rejects_discontiguous_batch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut db = open(&path);
        let h = header("s1");
        let error = append_batch(&mut db, &h, &[event(3, "turn/start")], false).unwrap_err();
        assert!(matches!(
            error,
            PersistenceError::Discontiguous {
                expected: 0,
                got: 3,
                ..
            }
        ));
        assert!(load_stored(&db, &store_id(&db), &SessionId("s1".into()))
            .unwrap()
            .is_none());
    }

    fn chunk_event(seq: u64, time: u64, text: &str) -> SessionEvent {
        SessionEvent {
            seq,
            time,
            event_type: "assistant/chunk".into(),
            data: JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(1)),
                ("step".into(), JsonValue::UInt(1)),
                (
                    "chunk".into(),
                    JsonValue::Object(vec![
                        ("type".into(), JsonValue::String("reasoning-delta".into())),
                        ("index".into(), JsonValue::UInt(0)),
                        ("text".into(), JsonValue::String(text.into())),
                    ]),
                ),
            ]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        }
    }

    #[test]
    fn chunk_runs_pack_physically_and_round_trip_logically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut db = open(&path);
        let id = SessionId("s1".into());
        let h = header("s1");
        // 50 same-block deltas pack into ONE physical row; the logical
        // stream stays 50 events and byte-identical on load.
        let run: Vec<SessionEvent> = (0..50)
            .map(|k| chunk_event(k, 1000 + k * 7, &format!("tok-{k}")))
            .collect();
        append_batch(&mut db, &h, &run, false).unwrap();
        let physical: i64 = db
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(physical, 1, "50-member run must occupy one physical row");
        let stored = load_stored(&db, &store_id(&db), &id).unwrap().unwrap();
        assert_eq!(stored.events.len(), 50);
        for (got, want) in stored.events.iter().zip(&run) {
            assert_eq!(got.seq, want.seq);
            assert_eq!(got.time, want.time);
            assert_eq!(
                got.data.to_json_string().unwrap(),
                want.data.to_json_string().unwrap()
            );
        }
        // The next append continues after the packed run's LAST member, not
        // after its keyed first seq.
        append_batch(&mut db, &h, &[event(50, "assistant/message")], true).unwrap();
        let stored = load_stored(&db, &store_id(&db), &id).unwrap().unwrap();
        assert_eq!(stored.events.len(), 51);
        assert_eq!(stored.events[50].event_type, "assistant/message");
        // A wrong next-seq still rejects as discontiguous (packed tail aware).
        let error = append_batch(&mut db, &h, &[event(52, "turn/end")], true).unwrap_err();
        assert!(matches!(error, PersistenceError::Discontiguous { .. }));
    }

    #[test]
    fn torn_packed_tail_repairs_by_dropping_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut db = open(&path);
        let id = SessionId("s1".into());
        let h = header("s1");
        let mut events = vec![event(0, "turn/start")];
        events.extend((0..10).map(|k| chunk_event(k + 1, 2000 + k, "t")));
        append_batch(&mut db, &h, &events, false).unwrap();
        // Corrupt the packed physical row (seq 1) behind the store's back.
        db.execute("UPDATE events SET data = '{torn' WHERE seq = 1", [])
            .unwrap();
        let stored = load_stored(&db, &store_id(&db), &id).unwrap().unwrap();
        assert_eq!(stored.torn_from, Some(1));
        assert_eq!(stored.events.len(), 1, "repair drops the whole torn run");
    }

    #[test]
    fn append_increments_revision_and_materializes_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut db = open(&path);
        let id = SessionId("s1".into());
        let h = header("s1");
        append_batch(
            &mut db,
            &h,
            &[event(0, "turn/start"), event(1, "user/message")],
            false,
        )
        .unwrap();
        let r1 = read_stored_revision(&db, &store_id(&db), &id)
            .unwrap()
            .unwrap();
        append_batch(&mut db, &h, &[event(2, "assistant/message")], true).unwrap();
        let r2 = read_stored_revision(&db, &store_id(&db), &id)
            .unwrap()
            .unwrap();
        assert_ne!(r1, r2);
        let stored = load_stored(&db, &store_id(&db), &id).unwrap().unwrap();
        assert_eq!(stored.header.seed_length, None);
        assert_eq!(stored.events.len(), 3);
    }

    #[test]
    fn surface_op_and_source_seqs_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut db = open(&path);
        let h = header("s1");
        let mut e = event(0, "assistant/message");
        e.surface_op = Some(SurfaceOp::Replace { start: 0, end: 1 });
        e.source_event_seqs = vec![0, 1];
        append_batch(&mut db, &h, &[e], false).unwrap();
        let stored = load_stored(&db, &store_id(&db), &SessionId("s1".into()))
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.events[0].surface_op,
            Some(SurfaceOp::Replace { start: 0, end: 1 })
        );
        assert_eq!(stored.events[0].source_event_seqs, vec![0, 1]);
    }

    #[test]
    fn multi_batch_appends_stay_contiguous_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut db = open(&path);
        let id = SessionId("s1".into());
        let h = header("s1");
        append_batch(
            &mut db,
            &h,
            &[event(0, "turn/start"), event(1, "user/message")],
            false,
        )
        .unwrap();
        append_batch(&mut db, &h, &[event(2, "assistant/message")], true).unwrap();
        append_batch(&mut db, &h, &[event(3, "turn/end")], true).unwrap();
        let stored = load_stored(&db, &store_id(&db), &id).unwrap().unwrap();
        let seqs = stored.events.iter().map(|e| e.seq).collect::<Vec<_>>();
        assert_eq!(seqs, vec![0, 1, 2, 3]);
    }

    #[test]
    fn remove_cascades_events_and_hides_from_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut db = open(&path);
        let id = SessionId("s1".into());
        let h = header("s1");
        append_batch(
            &mut db,
            &h,
            &[event(0, "turn/start"), event(1, "user/message")],
            false,
        )
        .unwrap();
        remove_session(&db, &id).unwrap();
        assert!(load_stored(&db, &store_id(&db), &id).unwrap().is_none());
        assert!(list_sessions(&db).unwrap().is_empty());
        let orphan_events: i64 = db
            .query_row(
                "SELECT count(*) FROM events WHERE session_id = ?1",
                [&id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orphan_events, 0, "FK cascade must remove events");
    }

    #[test]
    fn header_fields_round_trip_through_storage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut db = open(&path);
        let id = SessionId("s1".into());
        let mut h = header("s1");
        h.version = 0;
        h.parent_session = Some(SessionId("parent".into()));
        h.seed_length = Some(2);
        h.origin = Some(starriver_kernel::session::SessionOrigin::Subagent);
        h.delegation_depth = Some(1);
        h.agent_preset = Some("coder".into());
        append_batch(&mut db, &h, &[event(0, "turn/start")], false).unwrap();
        let stored = load_stored(&db, &store_id(&db), &id).unwrap().unwrap();
        assert_eq!(
            stored.header.parent_session,
            Some(SessionId("parent".into()))
        );
        assert_eq!(stored.header.seed_length, Some(2));
        assert_eq!(
            stored.header.origin,
            Some(starriver_kernel::session::SessionOrigin::Subagent)
        );
        assert_eq!(stored.header.delegation_depth, Some(1));
        assert_eq!(stored.header.agent_preset.as_deref(), Some("coder"));
        assert_eq!(stored.header.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn torn_tail_truncates_only_after_corrupt_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut db = open(&path);
        let id = SessionId("s1".into());
        let h = header("s1");
        append_batch(
            &mut db,
            &h,
            &[
                event(0, "turn/start"),
                event(1, "user/message"),
                event(2, "assistant/message"),
            ],
            false,
        )
        .unwrap();
        db.execute("UPDATE events SET data = '{torn' WHERE seq = 2", [])
            .unwrap();
        let stored = load_stored(&db, &store_id(&db), &id).unwrap().unwrap();
        assert_eq!(stored.torn_from, Some(2));
        assert_eq!(stored.events.len(), 2);
        // Truncate through the store helper inside a transaction.
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        truncate_torn_tail(&tx, &id, 2).unwrap();
        tx.commit().unwrap();
        let after = load_stored(&db, &store_id(&db), &id).unwrap().unwrap();
        assert_eq!(after.torn_from, None);
        assert_eq!(after.events.len(), 2);
    }

    fn big_tool_result_event(seq: u64, text: &str) -> SessionEvent {
        SessionEvent {
            seq,
            time: 1,
            event_type: "tool/result".into(),
            data: JsonValue::Object(vec![
                ("turn".into(), JsonValue::UInt(0)),
                ("step".into(), JsonValue::UInt(0)),
                (
                    "message".into(),
                    JsonValue::Object(vec![(
                        "content".into(),
                        JsonValue::Array(vec![JsonValue::Object(vec![(
                            "text".into(),
                            JsonValue::String(text.into()),
                        )])]),
                    )]),
                ),
            ]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        }
    }

    #[test]
    fn large_event_bodies_compress_and_round_trip_transparently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut db = open(&path);
        let id = SessionId("s1".into());
        let h = header("s1");
        let big_text = "tool output line with plenty of repetition\n".repeat(200);
        let events = vec![
            event(0, "turn/start"),
            big_tool_result_event(1, &big_text),
            event(2, "turn/end"),
        ];
        append_batch(&mut db, &h, &events, false).unwrap();
        // Physical storage: small rows plain, the big row compressed.
        let texts: Vec<String> = {
            let mut stmt = db
                .prepare("SELECT data FROM events WHERE session_id = 's1' ORDER BY seq")
                .unwrap();
            stmt.query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert!(!compression::is_compressed_text(&texts[0]));
        assert!(
            compression::is_compressed_text(&texts[1]),
            "big row compressed"
        );
        assert!(!compression::is_compressed_text(&texts[2]));
        // Logical load: byte-identical round trip.
        let stored = load_stored(&db, &store_id(&db), &id).unwrap().unwrap();
        assert_eq!(stored.events.len(), 3);
        assert_eq!(
            stored.events[1].data.to_json_string().unwrap(),
            events[1].data.to_json_string().unwrap()
        );
    }

    #[test]
    fn packed_row_payloads_compress_and_next_seq_stays_correct() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut db = open(&path);
        let id = SessionId("s1".into());
        let h = header("s1");
        // A long same-block text-delta run packs into one row whose payload
        // exceeds the compression threshold.
        let run: Vec<SessionEvent> = (0..120)
            .map(|k| {
                chunk_event(
                    k,
                    1000 + k,
                    "a sufficiently long delta payload segment for zstd to bite ",
                )
            })
            .collect();
        append_batch(&mut db, &h, &run, false).unwrap();
        let packed_text: String = db
            .query_row(
                "SELECT data FROM events WHERE session_id = 's1' ORDER BY seq LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(compression::is_compressed_text(&packed_text));
        let stored = load_stored(&db, &store_id(&db), &id).unwrap().unwrap();
        assert_eq!(stored.events.len(), 120);
        for (got, want) in stored.events.iter().zip(&run) {
            assert_eq!(
                got.data.to_json_string().unwrap(),
                want.data.to_json_string().unwrap()
            );
        }
        // next_seq decodes the compressed packed tail: appending at 120
        // succeeds, at 121 rejects as discontiguous.
        append_batch(&mut db, &h, &[event(120, "turn/end")], true).unwrap();
        let error = append_batch(&mut db, &h, &[event(122, "x")], true).unwrap_err();
        assert!(matches!(error, PersistenceError::Discontiguous { .. }));
    }

    #[test]
    fn compressed_marker_corruption_at_tail_is_torn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut db = open(&path);
        let id = SessionId("s1".into());
        let h = header("s1");
        let big_text = "repetitive payload for compression ".repeat(300);
        append_batch(
            &mut db,
            &h,
            &[event(0, "turn/start"), big_tool_result_event(1, &big_text)],
            false,
        )
        .unwrap();
        // Tear the compressed frame behind the store's back.
        db.execute(
            "UPDATE events SET data = 'zstd1:truncated' WHERE seq = 1",
            [],
        )
        .unwrap();
        let stored = load_stored(&db, &store_id(&db), &id).unwrap().unwrap();
        assert_eq!(stored.torn_from, Some(1), "torn compressed tail repairs");
        assert_eq!(stored.events.len(), 1);
    }

    #[test]
    fn ignorable_flagged_rows_restore_unchanged_and_stay_appendable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let mut db = open(&path);
        let id = SessionId("s1".into());
        let h = header("s1");
        // Envelope rows plus a packed chunk run: both physical shapes a
        // future writer might flag `ignorable` (WI-27 semantics).
        let mut batch = vec![event(0, "turn/start"), event(1, "user/message")];
        batch.extend((0..6).map(|k| chunk_event(k + 2, 1000 + k, "tok")));
        batch.push(event(8, "turn/end"));
        append_batch(&mut db, &h, &batch, false).unwrap();
        // Flag every physical row with the domain the CHECK allows (0/1 mix).
        db.execute("UPDATE events SET ignorable = 1 - seq % 2", [])
            .unwrap();
        // Restore is tolerant: the flag is query metadata, never log data —
        // the logical view is identical to an unflagged library.
        let stored = load_stored(&db, &store_id(&db), &id).unwrap().unwrap();
        assert_eq!(stored.events.len(), batch.len());
        assert_eq!(stored.events[8].event_type, "turn/end");
        // Contiguity bookkeeping (next_seq, packed tail included) tolerates
        // flagged rows: the next append still lands at the right seq.
        append_batch(&mut db, &h, &[event(9, "turn/start")], true).unwrap();
        let stored = load_stored(&db, &store_id(&db), &id).unwrap().unwrap();
        assert_eq!(stored.events.len(), 10);
        // The CHECK domain itself is enforced by the schema.
        assert!(
            db.execute("UPDATE events SET ignorable = 2 WHERE seq = 0", [])
                .is_err(),
            "ignorable domain is NULL/0/1 by CHECK"
        );
    }
}

// ── Round-6 regression: format version gate ──
#[cfg(test)]
mod round6_tests {
    use super::*;
    use starriver_kernel::persistence::PersistenceError;

    fn tempdb() -> (tempfile::TempDir, rusqlite::Connection, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("round6.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        crate::schema::open_owned(&conn, &path).unwrap();
        (dir, conn, path)
    }

    #[test]
    fn foreign_format_version_is_rejected_on_load() {
        let (_dir, conn, _path) = tempdb();
        let id = starriver_kernel::session::SessionId("s-v9".into());
        conn.execute(
            "INSERT INTO sessions (id, version, created_at, incarnation, revision) \
             VALUES (?1, 9, 1, 'i0', 0)",
            [&id.0],
        )
        .unwrap();
        let err = load_stored(&conn, "round6", &id).unwrap_err();
        assert!(
            matches!(err, PersistenceError::FormatUnsupported(_)),
            "version 9 must be rejected (got {err:?})"
        );
    }
}
