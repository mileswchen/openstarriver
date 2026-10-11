//! Two connections over the same database file: transaction-level seq checks,
//! repair staleness, and revision-driven cache invalidation.

use rusqlite::Connection;
use starriver_kernel::json::JsonValue;
use starriver_kernel::session::{SessionEvent, SessionHeader, SessionId};
use starriver_session_persistence::{schema, store};

fn header(id: &str) -> SessionHeader {
    let mut h = SessionHeader::new(SessionId(id.into()));
    h.cwd = Some("/tmp".into());
    h
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

fn store_id(db: &Connection) -> String {
    db.query_row("SELECT store_id FROM persistence_state", [], |row| {
        row.get(0)
    })
    .unwrap()
}

#[test]
fn second_writer_conflicts_on_same_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let mut a = Connection::open(&path).unwrap();
    schema::open_owned(&a, &path).unwrap();
    let _id = SessionId("s1".into());
    let h = header("s1");
    store::append_batch(&mut a, &h, &[event(0, "turn/start")], false).unwrap();
    // Connection B also starts at seq 0 — but A already consumed it, so B's
    // batch is rejected as discontiguous at the transaction level.
    let mut b = Connection::open(&path).unwrap();
    schema::open_owned(&b, &path).unwrap();
    let error = store::append_batch(
        &mut b,
        &h,
        &[event(0, "turn/start"), event(1, "user/message")],
        false,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        starriver_kernel::persistence::PersistenceError::Discontiguous { .. }
    ));
}

#[test]
fn revision_reads_see_other_writers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let mut a = Connection::open(&path).unwrap();
    schema::open_owned(&a, &path).unwrap();
    let sid = store_id(&a);
    let id = SessionId("s1".into());
    let h = header("s1");
    store::append_batch(&mut a, &h, &[event(0, "turn/start")], false).unwrap();
    let r1 = store::read_stored_revision(&a, &sid, &id).unwrap().unwrap();
    let mut b = Connection::open(&path).unwrap();
    schema::open_owned(&b, &path).unwrap();
    store::append_batch(&mut b, &h, &[event(1, "user/message")], true).unwrap();
    let r2 = store::read_stored_revision(&a, &sid, &id).unwrap().unwrap();
    assert_ne!(r1, r2);
}

#[test]
fn mutation_rejects_external_schema_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    let mut db = Connection::open(&path).unwrap();
    schema::open_owned(&db, &path).unwrap();
    let id = SessionId("s1".into());
    let h = header("s1");
    // Simulate an external writer changing app id after open.
    db.execute_batch("PRAGMA application_id = 999;").unwrap();
    let result = store::append_batch(&mut db, &h, &[event(0, "turn/start")], false);
    assert!(result.is_err());
    assert_eq!(id.0, "s1");
}
