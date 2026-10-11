//! Append-only JSONL session backend: one file per session plus a
//! store-level metadata file. The second backend after SQLite (WI-25),
//! selected at construction time via [`BackendKind`]. Durability is
//! write-through with `fsync`; behavior at the kernel `SessionPersistence`
//! seam matches the SQLite backend (lazy materialization, contiguous
//! append validation, torn-tail repair on load, loud corruption).
//!
//! Layout:
//! - `<root>/store.json`    — store metadata: format, version, store id
//! - `<root>/sessions/<enc(id)>.jsonl` — header line + event lines

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cordis_rslite::BoxFuture;
use starriver_kernel::json::JsonValue;
use starriver_kernel::persistence::{PersistenceError, SessionInspection, SessionPersistence};
use starriver_kernel::session::{SessionEvent, SessionHeader, SessionId, SessionOrigin};
use tokio_util::sync::CancellationToken;

/// `store.json` format tag.
const FORMAT: &str = "starriver-session-jsonl";
/// On-disk layout version (bump on breaking format changes; open rejects
/// mismatches instead of guessing, mirroring SQLite `user_version`).
const LAYOUT_VERSION: u64 = 1;

struct Shared {
    root: PathBuf,
    /// Next logical seq per materialized session, recovered from disk on
    /// open and maintained on append (in-process contiguity precheck).
    next_seq: HashMap<SessionId, u64>,
}

/// JSONL append-only session backend.
pub struct JsonlSessionPersistence {
    shared: Mutex<Shared>,
}

impl JsonlSessionPersistence {
    /// Open (or initialize) a JSONL store rooted at `root`. An existing
    /// `store.json` with a different format/version rejects.
    pub fn open(root: PathBuf) -> Result<Self, String> {
        std::fs::create_dir_all(root.join("sessions")).map_err(|e| e.to_string())?;
        let meta_path = root.join("store.json");
        if meta_path.exists() {
            let text = std::fs::read_to_string(&meta_path).map_err(|e| e.to_string())?;
            let meta = JsonValue::from_json_str(&text).map_err(|e| e.to_string())?;
            let format = meta.as_str_at("format").unwrap_or_default();
            let version = meta.get("version").and_then(JsonValue::as_u64);
            if format != FORMAT || version != Some(LAYOUT_VERSION) {
                return Err(format!(
                    "JSONL store at {} has format {format:?} version {version:?}, \
                     expected {FORMAT:?} v{LAYOUT_VERSION}",
                    root.display()
                ));
            }
        } else {
            let meta = JsonValue::Object(vec![
                ("format".into(), JsonValue::String(FORMAT.into())),
                ("version".into(), JsonValue::UInt(LAYOUT_VERSION)),
                (
                    "storeId".into(),
                    JsonValue::String(uuid::Uuid::new_v4().to_string()),
                ),
            ]);
            let text = meta.to_json_string().map_err(|e| e.to_string())?;
            let mut file = std::fs::File::create(&meta_path).map_err(|e| e.to_string())?;
            file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
            file.write_all(b"\n").map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
        }
        // Recover next-seq per session by scanning each file's tail.
        let mut next_seq = HashMap::new();
        let dir = root.join("sessions");
        let mut entries = std::fs::read_dir(&dir).map_err(|e| e.to_string())?;
        while let Some(entry) = entries.next() {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            let is_session = matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("jsonl") | Some("zst")
            );
            if !is_session {
                continue;
            }
            if let Ok(Some(header)) = read_header(&path) {
                // BUG-57: a header-only file (zero-event append) is still a
                // materialized session — not registering it made the next
                // append write a SECOND header line and the session unreadable.
                let next = read_last_event_seq(&path).map_or(0, |last| last + 1);
                next_seq.insert(header.id.clone(), next);
            }
        }
        // WI-100/BUG-89: cold-log compression is this backend's concern —
        // the jsonl layout owns <root>/sessions. One detached maintenance
        // thread per open: an immediate best-effort pass, then every 6h.
        // The on-disk run marker serializes concurrent passes (including
        // across processes); hot files are never touched.
        {
            let root = root.clone();
            let _ = std::thread::Builder::new()
                .name("starriver-cold-compress".into())
                .spawn(move || loop {
                    let _ = crate::cold_compress::compress_cold_sessions(&root);
                    std::thread::sleep(std::time::Duration::from_secs(6 * 3600));
                });
        }
        Ok(Self {
            shared: Mutex::new(Shared { root, next_seq }),
        })
    }

    fn session_path(root: &Path, id: &SessionId) -> PathBuf {
        root.join("sessions")
            .join(format!("{}.jsonl", encode_id(&id.0)))
    }

    /// The compressed twin of a session log (cold compression target).
    fn compressed_path(root: &Path, id: &SessionId) -> PathBuf {
        Self::session_path(root, id).with_extension("jsonl.zst")
    }

    /// The live file for a session: the plain log when it exists, else its
    /// cold-compressed twin (BUG-89 — the seam previously only ever looked
    /// at the plain path, making compressed sessions vanish from list/load).
    fn find_session_path(root: &Path, id: &SessionId) -> Option<PathBuf> {
        let plain = Self::session_path(root, id);
        if plain.exists() {
            return Some(plain);
        }
        let zst = Self::compressed_path(root, id);
        if zst.exists() {
            return Some(zst);
        }
        None
    }

    /// Turn a compressed session back into its plain appendable log
    /// (BUG-89): decode, write to a temp sibling, fsync, rename over the
    /// plain path, then drop the .zst. Contiguity is line-preserving, so
    /// the in-memory next-seq stays valid.
    fn rematerialize(root: &Path, id: &SessionId) -> Result<PathBuf, PersistenceError> {
        let plain = Self::session_path(root, id);
        let zst = Self::compressed_path(root, id);
        let text = read_text_maybe_compressed(&zst)
            .map_err(|e| PersistenceError::Io(e.to_string()))?;
        let tmp = plain.with_extension("jsonl.incoming");
        std::fs::write(&tmp, text.as_bytes())
            .map_err(|e| PersistenceError::Io(e.to_string()))?;
        if let Ok(file) = std::fs::File::open(&tmp) {
            let _ = file.sync_all();
        }
        std::fs::rename(&tmp, &plain)
            .map_err(|e| PersistenceError::Io(e.to_string()))?;
        let _ = std::fs::remove_file(&zst);
        Ok(plain)
    }

    /// Synchronous load with durable torn-tail repair (seam `load` body).
    fn load_sync(&self, id: &SessionId) -> Result<SessionInspection, PersistenceError> {
        let root = {
            let guard = self
                .shared
                .lock()
                .map_err(|_| PersistenceError::Io("poisoned".into()))?;
            guard.root.clone()
        };
        let Some(path) = JsonlSessionPersistence::find_session_path(&root, id) else {
            return Err(PersistenceError::NotFound(id.clone()));
        };
        let (header, events, torn) = match read_session_file(&path)? {
            Some(read) => read,
            None => return Err(PersistenceError::NotFound(id.clone())),
        };
        if let Some(first_torn_line) = torn {
            // A compressed log must be plain before it can be rewritten.
            if path.extension().and_then(|e| e.to_str()) == Some("zst") {
                JsonlSessionPersistence::rematerialize(&root, id)?;
            }
            repair_torn_tail(&root, id, first_torn_line)?;
            // Refresh the in-memory next-seq after the durable repair.
            if let Ok(mut guard) = self.shared.lock() {
                guard.next_seq.insert(id.clone(), events.len() as u64);
            }
        }
        // Parity with the SQLite load path: the seam's load returns the
        // repaired log as stored; synthesizing crash closers belongs to
        // `prepare` (the SQLite extension interface), not the seam.
        Ok(SessionInspection { header, events })
    }

    /// Synchronous list: header line per session file, created_at order.
    fn list_sync(&self) -> Result<Vec<SessionHeader>, PersistenceError> {
        let root = {
            let guard = self
                .shared
                .lock()
                .map_err(|_| PersistenceError::Io("poisoned".into()))?;
            guard.root.clone()
        };
        let dir = root.join("sessions");
        let entries = std::fs::read_dir(&dir).map_err(|e| PersistenceError::Io(e.to_string()))?;
        let mut headers = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        for entry in entries.flatten() {
            let path = entry.path();
            match path.extension().and_then(|e| e.to_str()) {
                Some("jsonl") => {}
                // BUG-89: cold-compressed sessions stay listed; when the
                // plain twin also exists (crash between .zst write and
                // unlink), the plain log is authoritative for that id.
                Some("zst") => {
                    let plain_twin = path.with_extension("");
                    let plain_twin = plain_twin.with_extension("jsonl");
                    if plain_twin.exists() {
                        continue;
                    }
                }
                _ => continue,
            }
            match read_header(&path) {
                Ok(Some(header)) => {
                    if seen_ids.insert(header.id.0.clone()) {
                        headers.push(header);
                    }
                }
                Ok(None) => {}
                Err(error) => return Err(PersistenceError::Corruption(error)),
            }
        }
        headers.sort_by_key(|header| header.created_at);
        Ok(headers)
    }
}

/// Percent-encode everything outside the filename-safe set so a hostile or
/// exotic session id can never traverse directories.
fn encode_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for byte in id.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn header_wire(header: &SessionHeader) -> JsonValue {
    let mut fields = vec![
        (
            "version".to_string(),
            JsonValue::UInt(header.version as u64),
        ),
        ("id".to_string(), JsonValue::String(header.id.0.clone())),
        ("createdAt".to_string(), JsonValue::UInt(header.created_at)),
    ];
    if let Some(cwd) = &header.cwd {
        fields.push(("cwd".into(), JsonValue::String(cwd.clone())));
    }
    if let Some(parent) = &header.parent_session {
        fields.push(("parentSession".into(), JsonValue::String(parent.0.clone())));
    }
    if let Some(seed_length) = header.seed_length {
        fields.push(("seedLength".into(), JsonValue::UInt(seed_length)));
    }
    if header.origin.is_some() {
        fields.push(("origin".into(), JsonValue::String("subagent".into())));
    }
    if let Some(depth) = header.delegation_depth {
        fields.push(("delegationDepth".into(), JsonValue::UInt(depth as u64)));
    }
    if let Some(preset) = &header.agent_preset {
        fields.push(("agentPreset".into(), JsonValue::String(preset.clone())));
    }
    JsonValue::Object(fields)
}

fn header_from_wire(value: &JsonValue, id: SessionId) -> Result<SessionHeader, String> {
    let version = value
        .get("version")
        .and_then(JsonValue::as_u64)
        .ok_or("header missing version")?;
    if version > u8::MAX as u64 {
        return Err(format!("header version {version} out of range"));
    }
    let created_at = value
        .get("createdAt")
        .and_then(JsonValue::as_u64)
        .ok_or("header missing createdAt")?;
    Ok(SessionHeader {
        version: version as u8,
        id,
        created_at,
        cwd: value
            .get("cwd")
            .and_then(JsonValue::as_str)
            .map(str::to_string),
        parent_session: value
            .get("parentSession")
            .and_then(JsonValue::as_str)
            .map(|s| SessionId(s.to_string())),
        seed_length: value.get("seedLength").and_then(JsonValue::as_u64),
        origin: value
            .as_str_at("origin")
            .filter(|o| *o == "subagent")
            .map(|_| SessionOrigin::Subagent),
        delegation_depth: value
            .get("delegationDepth")
            .and_then(JsonValue::as_u64)
            .map(|v| v as u32),
        agent_preset: value
            .get("agentPreset")
            .and_then(JsonValue::as_str)
            .map(str::to_string),
    })
}

/// Read and parse a session file. Returns the header, the valid events, and
/// the line index of the first torn (unparseable) tail line when the damage
/// is at the tail; damage in the committed prefix rejects as corruption.
/// Read session text: plain `.jsonl` directly, `.jsonl.zst` (WI-100 cold
/// compression) transparently decompressed. Returns NotFound when absent.
pub(crate) fn read_text_maybe_compressed_pub(path: &Path) -> std::io::Result<String> {
    read_text_maybe_compressed(path)
}

fn read_text_maybe_compressed(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        let mut decoder = zstd::Decoder::new(&bytes[..])
            .map_err(|e| std::io::Error::other(format!("zstd decode: {e}")))?;
        let mut text = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut text)
            .map_err(|e| std::io::Error::other(format!("zstd read: {e}")))?;
        Ok(text)
    } else {
        String::from_utf8(bytes).map_err(|e| std::io::Error::other(e))
    }
}

fn read_session_file(
    path: &Path,
) -> Result<Option<(SessionHeader, Vec<SessionEvent>, Option<usize>)>, PersistenceError> {
    let text = match read_text_maybe_compressed(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(PersistenceError::Io(error.to_string())),
    };
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let Some(first) = lines.next() else {
        return Ok(None);
    };
    let header_line = JsonValue::from_json_str(first)
        .map_err(|e| PersistenceError::Corruption(format!("session header line invalid: {e}")))?;
    if header_line.as_str_at("kind") != Some("session") {
        return Err(PersistenceError::Corruption(
            "first line must be the session header".into(),
        ));
    }
    let header_value = header_line.get("header").ok_or_else(|| {
        PersistenceError::Corruption("session header line missing header field".into())
    })?;
    let id = header_value
        .as_str_at("id")
        .map(|s| SessionId(s.to_string()))
        .ok_or_else(|| PersistenceError::Corruption("session header missing id".into()))?;
    let header = header_from_wire(header_value, id).map_err(PersistenceError::Corruption)?;
    let mut events = Vec::new();
    for (index, line) in lines.enumerate() {
        let parsed = JsonValue::from_json_str(line);
        let value = match parsed {
            Ok(value) => value,
            Err(_) => {
                // A torn tail line is repairable; anything earlier is loud.
                return Ok(Some((header, events, Some(index + 1))));
            }
        };
        if value.as_str_at("kind") != Some("event") {
            return Err(PersistenceError::Corruption(format!(
                "line {} is neither a valid event nor a torn tail",
                index + 1
            )));
        }
        let Some(event_value) = value.get("event") else {
            return Err(PersistenceError::Corruption(
                "event line missing event field".into(),
            ));
        };
        // Strict field presence: `from_json` defaults missing fields, which
        // would mask corruption, so pre-validate the required envelope.
        if event_value
            .get("type")
            .and_then(JsonValue::as_str)
            .is_none()
            || event_value.get("seq").and_then(JsonValue::as_u64).is_none()
            || event_value
                .get("time")
                .and_then(JsonValue::as_u64)
                .is_none()
            || event_value.get("data").is_none()
        {
            return Err(PersistenceError::Corruption(
                "event line missing required envelope fields".into(),
            ));
        }
        let event = SessionEvent::from_json(event_value)
            .map_err(|e| PersistenceError::Corruption(e.to_string()))?;
        if event.seq != events.len() as u64 {
            return Err(PersistenceError::Corruption(format!(
                "event {} breaks seq contiguity (expected {})",
                event.seq,
                events.len()
            )));
        }
        events.push(event);
    }
    Ok(Some((header, events, None)))
}

/// Header line of a session file, or None when absent/unreadable-as-session.
fn read_header(path: &Path) -> Result<Option<SessionHeader>, String> {
    match read_session_file(path) {
        Ok(Some((header, _, _))) => Ok(Some(header)),
        Ok(None) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Last event seq of a session file (0 when only the header line exists).
fn read_last_event_seq(path: &Path) -> Option<u64> {
    let (_, events, _) = read_session_file(path).ok()??;
    events.last().map(|e| e.seq)
}

/// Durably rewrite a session file dropping every line at or after
/// `first_torn_line` (1-based line index of the first event line to drop).
fn repair_torn_tail(
    root: &Path,
    id: &SessionId,
    first_torn_line: usize,
) -> Result<(), PersistenceError> {
    let path = JsonlSessionPersistence::session_path(root, id);
    let text = std::fs::read_to_string(&path).map_err(|e| PersistenceError::Io(e.to_string()))?;
    // Line 0 is the header; event lines are 1-based. Keep lines [0,
    // first_torn_line) — BUG-58: the torn index counts NON-BLANK lines, so
    // the cut must be taken over the same filtered sequence (a raw take
    // shifted the cut point per blank line and deleted committed events).
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(first_torn_line)
        .collect();
    let mut file = std::fs::File::create(&path).map_err(|e| PersistenceError::Io(e.to_string()))?;
    for line in kept {
        file.write_all(line.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .map_err(|e| PersistenceError::Io(e.to_string()))?;
    }
    file.sync_all()
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    Ok(())
}

/// Test-only write path with a caller-controlled full header: the seam's
/// append fabricates a minimal header, while full-header round-trips (e.g.
/// the query module's filter fixtures) need controlled `created_at` /
/// `agent_preset`.
#[cfg(test)]
pub(crate) fn append_lines_for_test(
    root: &Path,
    header: &SessionHeader,
    events: &[SessionEvent],
) -> Result<(), PersistenceError> {
    append_lines(root, &header.id, header, events, false)
}

fn append_lines(
    root: &Path,
    id: &SessionId,
    header: &SessionHeader,
    events: &[SessionEvent],
    materialized: bool,
) -> Result<(), PersistenceError> {
    let path = JsonlSessionPersistence::session_path(root, id);
    let mut lines = String::new();
    if !materialized {
        let wire = JsonValue::Object(vec![
            ("kind".into(), JsonValue::String("session".into())),
            ("header".into(), header_wire(header)),
        ]);
        lines.push_str(
            &wire
                .to_json_string()
                .map_err(|e| PersistenceError::Corruption(e.to_string()))?,
        );
        lines.push('\n');
    }
    for event in events {
        let wire = JsonValue::Object(vec![
            ("kind".into(), JsonValue::String("event".into())),
            ("event".into(), event.to_json()),
        ]);
        lines.push_str(
            &wire
                .to_json_string()
                .map_err(|e| PersistenceError::Corruption(e.to_string()))?,
        );
        lines.push('\n');
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    file.write_all(lines.as_bytes())
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    // Write-through durability: the returned future implies fsynced data.
    file.sync_all()
        .map_err(|e| PersistenceError::Io(e.to_string()))?;
    Ok(())
}

impl SessionPersistence for JsonlSessionPersistence {
    fn create(&self, _header: SessionHeader) -> BoxFuture<Result<(), PersistenceError>> {
        // Lazy materialization: nothing durable until the first append.
        Box::pin(async { Ok(()) })
    }

    fn append(
        &self,
        id: SessionId,
        events: Vec<SessionEvent>,
        cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<(), PersistenceError>> {
        let result = (|| {
            let Ok(mut guard) = self.shared.lock() else {
                return Err(PersistenceError::Io("poisoned".into()));
            };
            let header = SessionHeader::new(id.clone());
            let id = header.id.clone();
            // Contiguity precheck against the in-memory next-seq map.
            let stored_next = guard.next_seq.get(&id).copied().unwrap_or(0);
            for (index, event) in events.iter().enumerate() {
                if event.seq != stored_next + index as u64 {
                    return Err(PersistenceError::Discontiguous {
                        id,
                        expected: stored_next + index as u64,
                        got: event.seq,
                        index,
                    });
                }
            }
            let materialized = guard.next_seq.contains_key(&id);
            // BUG-89: a materialized session whose live file is the
            // compressed twin must be re-materialized first — appending
            // header-less event lines to a fresh .jsonl while the .zst stays
            // double-registers the session and corrupts contiguity.
            if materialized
                && !JsonlSessionPersistence::session_path(&guard.root, &id).exists()
                && JsonlSessionPersistence::compressed_path(&guard.root, &id).exists()
            {
                if let Err(e) = JsonlSessionPersistence::rematerialize(&guard.root, &id) {
                    return Err(e);
                }
            }
            let result = append_lines(&guard.root, &id, &header, &events, materialized);
            if result.is_ok() {
                guard
                    .next_seq
                    .insert(id.clone(), stored_next + events.len() as u64);
            }
            result
        })();
        Box::pin(async move {
            if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
                return Err(PersistenceError::Canceled);
            }
            result
        })
    }

    fn load(
        &self,
        id: SessionId,
        _cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<SessionInspection, PersistenceError>> {
        let result = self.load_sync(&id);
        Box::pin(async move { result })
    }

    fn list(
        &self,
        _cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<Vec<SessionHeader>, PersistenceError>> {
        let result = self.list_sync();
        Box::pin(async move { result })
    }

    fn remove(
        &self,
        id: SessionId,
        _cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<(), PersistenceError>> {
        let result = {
            let Ok(mut guard) = self.shared.lock() else {
                return Box::pin(async { Err(PersistenceError::Io("poisoned".into())) });
            };
            guard.next_seq.remove(&id);
            let plain = JsonlSessionPersistence::session_path(&guard.root, &id);
            // BUG-89: a cold-compressed twin dies with its session.
            let zst = JsonlSessionPersistence::compressed_path(&guard.root, &id);
            let outcomes = [std::fs::remove_file(&plain), std::fs::remove_file(&zst)];
            outcomes
                .into_iter()
                .find_map(|r| match r {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        Some(PersistenceError::Io(error.to_string()))
                    }
                    _ => None,
                })
                .map_or(Ok(()), Err)
        };
        Box::pin(async move { result })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn header(id: &str) -> SessionHeader {
        let mut header = SessionHeader::new(SessionId(id.into()));
        header.cwd = Some("/tmp".into());
        header
    }

    fn open(root: &Path) -> JsonlSessionPersistence {
        JsonlSessionPersistence::open(root.to_path_buf()).unwrap()
    }

    #[tokio::test]
    async fn lazy_materialization_and_list_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        let h1 = header("s1");
        let h2 = {
            let mut h = header("s2");
            h.created_at = 42;
            h
        };
        store.create(h1.clone()).await.unwrap();
        store.create(h2.clone()).await.unwrap();
        assert!(store.list(None).await.unwrap().is_empty());
        store
            .append(SessionId("s2".into()), vec![event(0, "turn/start")], None)
            .await
            .unwrap();
        store
            .append(SessionId("s1".into()), vec![event(0, "turn/start")], None)
            .await
            .unwrap();
        let listed = store.list(None).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id.0, "s2", "list orders by created_at");
        // The seam's append materializes a minimal header (cwd is None),
        // matching SQLite; full-field headers round-trip via the header
        // line test below.
    }

    #[tokio::test]
    async fn reopen_recovers_next_seq_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = open(dir.path());
            store
                .append(
                    SessionId("s1".into()),
                    vec![event(0, "turn/start"), event(1, "user/message")],
                    None,
                )
                .await
                .unwrap();
        }
        let store = open(dir.path());
        // A fresh instance recovered the stored length: appending at 0 is
        // discontiguous, at 2 succeeds.
        let error = store
            .append(SessionId("s1".into()), vec![event(0, "x")], None)
            .await
            .unwrap_err();
        assert!(matches!(error, PersistenceError::Discontiguous { .. }));
        store
            .append(SessionId("s1".into()), vec![event(2, "turn/end")], None)
            .await
            .unwrap();
        let loaded = store.load(SessionId("s1".into()), None).await.unwrap();
        assert_eq!(loaded.events.len(), 3);
        assert_eq!(loaded.events[2].event_type, "turn/end");
    }

    #[tokio::test]
    async fn torn_tail_line_is_truncated_durably() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = open(dir.path());
            store
                .append(
                    SessionId("s1".into()),
                    vec![event(0, "turn/start"), event(1, "user/message")],
                    None,
                )
                .await
                .unwrap();
        }
        // Physically tear the last line (half-written append).
        let path = dir.path().join("sessions").join("s1.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines.pop();
        lines.push(r#"{"kind":"even"#);
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let store = open(dir.path());
        let loaded = store.load(SessionId("s1".into()), None).await.unwrap();
        assert_eq!(loaded.events.len(), 1);
        // The repair is durable: a fresh reader sees the truncated log.
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
    }

    #[tokio::test]
    async fn hostile_id_cannot_traverse_directories() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        store
            .append(
                SessionId("../../etc/passwd".into()),
                vec![event(0, "turn/start")],
                None,
            )
            .await
            .unwrap();
        let sessions_dir = dir.path().join("sessions");
        let names: Vec<String> = std::fs::read_dir(&sessions_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1);
        assert!(!names[0].contains('/'));
        // The header line keeps the true id; list still returns it.
        let listed = store.list(None).await.unwrap();
        assert_eq!(listed[0].id.0, "../../etc/passwd");
    }

    #[tokio::test]
    async fn remove_deletes_file_and_next_loads_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        store
            .append(SessionId("s1".into()), vec![event(0, "turn/start")], None)
            .await
            .unwrap();
        store.remove(SessionId("s1".into()), None).await.unwrap();
        assert!(matches!(
            store.load(SessionId("s1".into()), None).await.unwrap_err(),
            PersistenceError::NotFound(_)
        ));
        assert!(store.list(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn wrong_version_store_json_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(
            dir.path().join("store.json"),
            r#"{"format":"starriver-session-jsonl","version":99,"storeId":"x"}"#,
        )
        .unwrap();
        assert!(JsonlSessionPersistence::open(dir.path().into()).is_err());
    }

    #[tokio::test]
    async fn full_header_round_trips_through_the_header_line() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path());
        // Write through the internal path with a full header (the seam's
        // append carries only the id; the live path carries real headers).
        let mut header = header("s1");
        header.parent_session = Some(SessionId("parent".into()));
        header.seed_length = Some(2);
        header.origin = Some(SessionOrigin::Subagent);
        header.delegation_depth = Some(1);
        header.agent_preset = Some("coder".into());
        {
            let guard = store.shared.lock().unwrap();
            append_lines(
                &guard.root,
                &header.id,
                &header,
                &[event(0, "turn/start")],
                false,
            )
            .unwrap();
        }
        let loaded = store.load(SessionId("s1".into()), None).await.unwrap();
        assert_eq!(loaded.header.cwd.as_deref(), Some("/tmp"));
        assert_eq!(
            loaded.header.parent_session,
            Some(SessionId("parent".into()))
        );
        assert_eq!(loaded.header.seed_length, Some(2));
        assert_eq!(loaded.header.origin, Some(SessionOrigin::Subagent));
        assert_eq!(loaded.header.delegation_depth, Some(1));
        assert_eq!(loaded.header.agent_preset.as_deref(), Some("coder"));
        let listed = store.list(None).await.unwrap();
        assert_eq!(listed[0].agent_preset.as_deref(), Some("coder"));
    }

    #[tokio::test]
    async fn ignorable_wire_field_is_tolerated_on_restore() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = open(dir.path());
            store
                .append(
                    SessionId("s1".into()),
                    vec![event(0, "turn/start"), event(1, "user/message")],
                    None,
                )
                .await
                .unwrap();
        }
        // A future writer tags events ignorable in the durable wire envelope
        // (WI-27 semantics): restore must read them unchanged — the flag is
        // query metadata, never log data.
        let path = dir.path().join("sessions").join("s1.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let patched = text.replace(
            "\"event\":{\"type\"",
            "\"event\":{\"ignorable\":true,\"type\"",
        );
        assert_ne!(patched, text, "both event lines carry the flag");
        std::fs::write(&path, patched).unwrap();
        let store = open(dir.path());
        let loaded = store.load(SessionId("s1".into()), None).await.unwrap();
        assert_eq!(loaded.events.len(), 2);
        assert_eq!(loaded.events[1].event_type, "user/message");
        // Contiguity recovery after reopening tolerates flagged lines too.
        store
            .append(SessionId("s1".into()), vec![event(2, "turn/end")], None)
            .await
            .unwrap();
        let loaded = store.load(SessionId("s1".into()), None).await.unwrap();
        assert_eq!(loaded.events.len(), 3);
    }
}

// ── Round-5 regressions (BUG-57 / BUG-58): reopen + torn-tail repair ──
#[cfg(test)]
mod round5_tests {
    use super::*;

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

    fn open(root: &Path) -> JsonlSessionPersistence {
        JsonlSessionPersistence::open(root.to_path_buf()).unwrap()
    }

    /// BUG-57: an empty event batch materializes a header-only file; on
    /// reopen the zero-event session is not registered in next_seq, so the
    /// next append writes a SECOND header line and the session is
    /// permanently unreadable (Corruption).
    #[tokio::test]
    async fn empty_batch_then_reopen_does_not_corrupt_the_session() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = open(dir.path());
            store.append(SessionId("s1".into()), vec![], None).await.unwrap();
        }
        let store = open(dir.path());
        store
            .append(SessionId("s1".into()), vec![event(0, "turn/start")], None)
            .await
            .expect("append after reopen of a zero-event session");
        let loaded = store.load(SessionId("s1".into()), None).await
            .expect("the session must stay readable (no double header)");
        assert_eq!(loaded.events.len(), 1);
    }

    /// BUG-58: the torn-line index is computed over BLANK-FILTERED lines but
    /// the repair truncates RAW lines — one blank line shifts the cut by one
    /// and durably deletes a valid committed event.
    #[tokio::test]
    async fn torn_tail_repair_tolerates_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let id = SessionId("s-blank".into());
        {
            let store = open(dir.path());
            store
                .append(id.clone(), vec![event(0, "turn/start")], None)
                .await
                .unwrap();
        }
        // Inject blank lines BEFORE the committed event (external edit /
        // partial corruption) plus a torn final line: the filtered-index
        // torn marker then points past the event on raw lines.
        let path = JsonlSessionPersistence::session_path(dir.path(), &id);
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let mut rebuilt = String::new();
        rebuilt.push_str(lines[0]);
        rebuilt.push_str("\n\n\n"); // two blank lines before the event
        rebuilt.push_str(lines[1]);
        rebuilt.push_str("\n{torn");
        std::fs::write(&path, rebuilt).unwrap();

        let store = open(dir.path());
        let loaded = store.load(id.clone(), None).await
            .expect("torn tail is repairable");
        assert_eq!(
            loaded.events.len(),
            1,
            "the valid committed event survives the repair"
        );
        // And the repair is durable: the second load sees the same content
        // (not NotFound — the event must not have been dropped).
        let store2 = open(dir.path());
        let loaded2 = store2.load(id, None).await
            .expect("session still exists after repair");
        assert_eq!(loaded2.events.len(), 1);
    }

    /// BUG-89: a cold-compressed session stays visible, loadable and
    /// appendable through the seam — append re-materializes the plain log.
    #[tokio::test]
    async fn compressed_session_lists_loads_and_appends() {
        use crate::cold_compress::{compress_cold_sessions, SESSION_COLD_DAYS};

        let dir = tempfile::tempdir().unwrap();
        let id = SessionId("zst-1".into());
        {
            let store = open(dir.path());
            let fat = "x".repeat(2048);
            store
                .append(
                    id.clone(),
                    vec![
                        event(0, "turn/start"),
                        event(1, "user/message"),
                        // Push the log past the 1KB compression threshold.
                        SessionEvent {
                            seq: 2,
                            time: 1002,
                            event_type: "user/message".into(),
                            data: JsonValue::Object(vec![(
                                "payload".into(),
                                JsonValue::String(fat),
                            )]),
                            surface_op: None,
                            source_event_seqs: Vec::new(),
                        },
                    ],
                    None,
                )
                .await
                .unwrap();
        }
        let root = dir.path().to_path_buf();
        let plain = root.join("sessions").join("zst-1.jsonl");
        assert!(plain.exists(), "plain log materialized");

        // Age the log past the cold threshold and compress it.
        let old = std::time::SystemTime::now()
            - std::time::Duration::from_secs((SESSION_COLD_DAYS + 1) * 24 * 3600);
        let f = std::fs::File::options().write(true).open(&plain).unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        assert_eq!(compress_cold_sessions(&root), 1, "log compressed");
        assert!(!plain.exists(), "original replaced by the twin");
        assert!(root.join("sessions").join("zst-1.jsonl.zst").exists());

        // list: the compressed session stays listed.
        let store = open(&root);
        let listed = store.list(None).await.unwrap();
        assert!(
            listed.iter().any(|h| h.id.0 == "zst-1"),
            "compressed session listed: {listed:?}"
        );

        // load: reads through the twin transparently.
        let loaded = store.load(id.clone(), None).await.expect("load compressed");
        assert!(loaded.events.len() >= 2, "both events readable");

        // append: re-materializes the plain log, appends contiguously, drops
        // the twin.
        store
            .append(id.clone(), vec![event(3, "assistant/message")], None)
            .await
            .unwrap();
        assert!(plain.exists(), "plain log re-materialized");
        assert!(
            !root.join("sessions").join("zst-1.jsonl.zst").exists(),
            "twin dropped"
        );
        let reloaded = store.load(id.clone(), None).await.unwrap();
        let seqs: Vec<u64> = reloaded.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3], "append contiguous after rematerialize");

        // remove: both twins die with the session.
        store.remove(id.clone(), None).await.unwrap();
        assert!(!plain.exists());
        assert!(!root.join("sessions").join("zst-1.jsonl.zst").exists());
    }
}
