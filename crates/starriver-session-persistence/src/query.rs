//! Session query & projection (WI-24): time / agent-preset / text filters
//! plus a per-session summary projection, consumed over the kernel
//! [`SessionPersistence`] seam. Backend-agnostic by construction — SQLite
//! and JSONL (and any future backend) serve identical behavior because the
//! filters project onto `list()` + `load()` rather than backend internals.
//! Push-down (SQL `WHERE`, per-file scans) is a per-backend optimization
//! that can layer under this API later.

use starriver_kernel::persistence::{PersistenceError, SessionPersistence};
use starriver_kernel::session::{SessionEvent, SessionHeader, SessionId};
use tokio_util::sync::CancellationToken;

/// Filter over persisted sessions. All fields optional; absent fields do
/// not constrain. Bounds are inclusive Unix epoch milliseconds over the
/// session header's `created_at`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionQuery {
    /// Keep sessions created at or after this instant.
    pub created_after: Option<u64>,
    /// Keep sessions created at or before this instant.
    pub created_before: Option<u64>,
    /// Keep sessions whose header cites exactly this agent preset.
    pub agent_preset: Option<String>,
    /// Case-insensitive substring over the sessions' event payloads
    /// (serialized JSON of each event's `data`). Sessions whose log never
    /// matches are excluded.
    pub text: Option<String>,
}

impl SessionQuery {
    /// Keep every session (no constraints).
    pub fn all() -> Self {
        Self::default()
    }

    fn header_matches(&self, header: &SessionHeader) -> bool {
        if let Some(after) = self.created_after {
            if header.created_at < after {
                return false;
            }
        }
        if let Some(before) = self.created_before {
            if header.created_at > before {
                return false;
            }
        }
        if let Some(preset) = &self.agent_preset {
            if header.agent_preset.as_ref() != Some(preset) {
                return false;
            }
        }
        true
    }
}

/// Derived summary of one persisted session: the minimal projection the
/// session-list UIs need without loading full logs per render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: SessionId,
    pub created_at: u64,
    pub cwd: Option<String>,
    pub agent_preset: Option<String>,
    /// Total logical events (packed chunk rows expanded).
    pub event_count: u64,
    /// `turn/start` occurrences.
    pub turn_count: u64,
    /// `tool/call` occurrences.
    pub tool_call_count: u64,
    /// `tool/result` occurrences.
    pub tool_result_count: u64,
    /// Event type of the log tail, `None` for an empty log.
    pub last_event_type: Option<String>,
    /// `time` of the log tail, `None` for an empty log.
    pub last_activity: Option<u64>,
    /// True when the log rests on a balanced boundary (`turn/end`,
    /// `session/disposed`) or is empty — the "last state" bit.
    pub idle: bool,
}

/// Project a header + decoded event log into a summary. Pure: the same
/// input yields the same summary on every backend.
pub fn summarize(header: &SessionHeader, events: &[SessionEvent]) -> SessionSummary {
    let mut turn_count = 0u64;
    let mut tool_call_count = 0u64;
    let mut tool_result_count = 0u64;
    for event in events {
        match event.event_type.as_str() {
            "turn/start" => turn_count += 1,
            "tool/call" => tool_call_count += 1,
            "tool/result" => tool_result_count += 1,
            _ => {}
        }
    }
    let last = events.last();
    let last_event_type = last.map(|e| e.event_type.clone());
    let idle = match last {
        None => true,
        Some(event) => event.event_type == "turn/end" || event.event_type == "session/disposed",
    };
    SessionSummary {
        id: header.id.clone(),
        created_at: header.created_at,
        cwd: header.cwd.clone(),
        agent_preset: header.agent_preset.clone(),
        event_count: events.len() as u64,
        turn_count,
        tool_call_count,
        tool_result_count,
        last_event_type,
        last_activity: last.map(|e| e.time),
        idle,
    }
}

/// Case-insensitive substring match over the serialized event payloads.
fn log_matches_text(events: &[SessionEvent], needle: &str) -> bool {
    let needle = needle.to_lowercase();
    events.iter().any(|event| {
        event
            .data
            .to_json_string()
            .map(|text| text.to_lowercase().contains(&needle))
            .unwrap_or(false)
    })
}

/// Query persisted sessions and project each match to a summary. Order
/// follows the seam's `list()` (created_at ascending). `cancel` is honored
/// at entry and between sessions.
pub async fn query_sessions(
    store: &dyn SessionPersistence,
    query: &SessionQuery,
    cancel: Option<CancellationToken>,
) -> Result<Vec<SessionSummary>, PersistenceError> {
    if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
        return Err(PersistenceError::Canceled);
    }
    let headers = store.list(cancel.clone()).await?;
    let mut summaries = Vec::new();
    for header in headers {
        if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            return Err(PersistenceError::Canceled);
        }
        if !query.header_matches(&header) {
            continue;
        }
        let inspection = store.load(header.id.clone(), cancel.clone()).await?;
        if let Some(text) = &query.text {
            if !log_matches_text(&inspection.events, text) {
                continue;
            }
        }
        summaries.push(summarize(&header, &inspection.events));
    }
    Ok(summaries)
}

/// WI-107: one full-text search hit over a session's event payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSearchHit {
    pub id: SessionId,
    pub created_at: u64,
    /// Windowed excerpt of the first matching event's serialized payload,
    /// centered on the first match position.
    pub snippet: String,
    /// Total occurrences of the needle across all event payloads.
    pub matches: usize,
}

/// Window size (in chars) on each side of the match for the snippet.
const SNIPPET_RADIUS: usize = 48;

/// Count occurrences of `needle` (case-insensitive) in `hay`.
fn count_occurrences(hay: &str, needle_lower: &str) -> usize {
    if needle_lower.is_empty() {
        return 0;
    }
    let hay_lower = hay.to_lowercase();
    let mut count = 0usize;
    let mut at = 0usize;
    while let Some(found) = hay_lower[at..].find(needle_lower) {
        count += 1;
        at += found + needle_lower.len().max(1);
    }
    count
}

/// Build the snippet for the first event whose serialized payload matches.
fn snippet_for(events: &[SessionEvent], needle: &str) -> Option<(String, usize)> {
    let needle_lower = needle.to_lowercase();
    let mut total = 0usize;
    let mut first: Option<String> = None;
    for event in events {
        let Ok(text) = event.data.to_json_string() else {
            continue;
        };
        let hits = count_occurrences(&text, &needle_lower);
        total += hits;
        if hits > 0 && first.is_none() {
            let hay_lower = text.to_lowercase();
            let pos = hay_lower.find(&needle_lower).unwrap_or(0);
            // Slice over chars so multi-byte text never splits a code point.
            let chars: Vec<char> = text.chars().collect();
            let start_byte = text
                .char_indices()
                .nth(pos.saturating_sub(SNIPPET_RADIUS))
                .map(|(b, _)| b)
                .unwrap_or(0);
            let end_idx = (pos + needle.chars().count() + SNIPPET_RADIUS)
                .min(chars.len().saturating_sub(1));
            // Recompute the end byte from the char index.
            let end_byte = text
                .char_indices()
                .nth(end_idx + 1)
                .map(|(b, _)| b)
                .unwrap_or(text.len());
            let mut snippet: String = text[start_byte..end_byte].to_string();
            if start_byte > 0 {
                snippet.insert(0, '…');
            }
            if end_byte < text.len() {
                snippet.push('…');
            }
            snippet = snippet.replace('\n', " ");
            first = Some(snippet);
        }
    }
    first.map(|snippet| (snippet, total))
}

/// WI-107: full-text search across persisted sessions' event payloads —
/// backend-agnostic (list + load), same traversal shape as
/// [`query_sessions`]. Newest sessions first, at most `limit` hits.
pub async fn search_sessions(
    store: &dyn SessionPersistence,
    needle: &str,
    limit: usize,
    cancel: Option<CancellationToken>,
) -> Result<Vec<SessionSearchHit>, PersistenceError> {
    if needle.trim().is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
        return Err(PersistenceError::Canceled);
    }
    let headers = store.list(cancel.clone()).await?;
    let mut headers = headers;
    headers.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let mut hits = Vec::new();
    for header in headers {
        if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            return Err(PersistenceError::Canceled);
        }
        if hits.len() >= limit {
            break;
        }
        let inspection = store.load(header.id.clone(), cancel.clone()).await?;
        if let Some((snippet, matches)) = snippet_for(&inspection.events, needle) {
            hits.push(SessionSearchHit {
                id: header.id.clone(),
                created_at: header.created_at,
                snippet,
                matches,
            });
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonl;
    use crate::SqliteSessionPersistence;
    use starriver_kernel::json::JsonValue;

    fn text_event(seq: u64, event_type: &str, text: &str) -> SessionEvent {
        SessionEvent {
            seq,
            time: 10_000 + seq,
            event_type: event_type.into(),
            data: JsonValue::Object(vec![("text".into(), JsonValue::String(text.into()))]),
            surface_op: None,
            source_event_seqs: Vec::new(),
        }
    }

    fn header(id: &str, created_at: u64, preset: Option<&str>) -> SessionHeader {
        let mut header = SessionHeader::new(SessionId(id.into()));
        header.created_at = created_at;
        header.cwd = Some("/work".into());
        header.agent_preset = preset.map(str::to_string);
        header
    }

    /// A one-turn log with a tool round-trip and a resting tail.
    fn sample_log() -> Vec<SessionEvent> {
        vec![
            text_event(0, "turn/start", "begin"),
            text_event(1, "user/message", "please grep the REPO for zstd"),
            text_event(2, "assistant/message", "looking"),
            text_event(3, "tool/call", "grep"),
            text_event(4, "tool/result", "zstd hits found"),
            text_event(5, "turn/end", "completed"),
        ]
    }

    fn samples() -> [(&'static str, u64, Option<&'static str>); 3] {
        [
            ("old", 1_000, Some("reviewer")),
            ("mid", 2_000, Some("coder")),
            ("new", 3_000, None),
        ]
    }

    /// SQLite store with the three sample sessions written through the
    /// coordinator's full-header path (the seam fabricates minimal headers).
    async fn sqlite_with_samples() -> (tempfile::TempDir, SqliteSessionPersistence) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionPersistence::open(dir.path().join("sessions.db")).unwrap();
        for (id, created_at, preset) in samples() {
            let h = header(id, created_at, preset);
            store
                .coordinator()
                .append(h.id.clone(), h, sample_log())
                .await
                .unwrap();
            store.coordinator().flush().await.unwrap();
        }
        (dir, store)
    }

    /// JSONL store with the same sessions written through the backend's
    /// full-header line path.
    fn jsonl_with_samples() -> (tempfile::TempDir, jsonl::JsonlSessionPersistence) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let store = jsonl::JsonlSessionPersistence::open(root.clone()).unwrap();
        for (id, created_at, preset) in samples() {
            let h = header(id, created_at, preset);
            jsonl::append_lines_for_test(&root, &h, &sample_log()).unwrap();
        }
        (dir, store)
    }

    /// The whole filter + projection contract, run against any backend.
    async fn assert_query_semantics(store: &dyn SessionPersistence) {
        // Inclusive window [1500, 2500] keeps only "mid".
        let summaries = query_sessions(
            store,
            &SessionQuery {
                created_after: Some(1_500),
                created_before: Some(2_500),
                ..SessionQuery::all()
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id.0, "mid");

        // Agent preset is an exact match.
        let summaries = query_sessions(
            store,
            &SessionQuery {
                agent_preset: Some("reviewer".into()),
                ..SessionQuery::all()
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id.0, "old");

        // Unfiltered query lists all three in created_at order, each
        // carrying the full projection of the sample log.
        let summaries = query_sessions(store, &SessionQuery::all(), None)
            .await
            .unwrap();
        let ids: Vec<&str> = summaries.iter().map(|s| s.id.0.as_str()).collect();
        assert_eq!(ids, vec!["old", "mid", "new"]);
        for summary in &summaries {
            assert_eq!(summary.event_count, 6);
            assert_eq!(summary.turn_count, 1);
            assert_eq!(summary.tool_call_count, 1);
            assert_eq!(summary.tool_result_count, 1);
            assert_eq!(summary.last_event_type.as_deref(), Some("turn/end"));
            assert_eq!(summary.last_activity, Some(10_005));
            assert!(summary.idle, "balanced tail is idle");
            assert_eq!(summary.cwd.as_deref(), Some("/work"));
        }

        // Text filter: case-insensitive substring over payloads. Every log
        // contains "zstd", nobody contains the miss term, and filters
        // compose (text + preset narrows to one session).
        let hit = query_sessions(
            store,
            &SessionQuery {
                text: Some("ZSTD".into()),
                ..SessionQuery::all()
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(hit.len(), 3);
        let miss = query_sessions(
            store,
            &SessionQuery {
                text: Some("nothing-matches".into()),
                ..SessionQuery::all()
            },
            None,
        )
        .await
        .unwrap();
        assert!(miss.is_empty());
        let both = query_sessions(
            store,
            &SessionQuery {
                text: Some("grep the repo".into()),
                agent_preset: Some("coder".into()),
                ..SessionQuery::all()
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].id.0, "mid");

        // A canceled token fails fast.
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = query_sessions(store, &SessionQuery::all(), Some(cancel))
            .await
            .unwrap_err();
        assert!(matches!(error, PersistenceError::Canceled));
    }

    #[tokio::test]
    async fn sqlite_backend_serves_the_query_contract() {
        let (_dir, store) = sqlite_with_samples().await;
        assert_query_semantics(&store).await;
    }

    #[tokio::test]
    async fn jsonl_backend_serves_the_query_contract() {
        let (_dir, store) = jsonl_with_samples();
        assert_query_semantics(&store).await;
    }

    #[test]
    fn summarize_handles_empty_and_midturn_logs() {
        let h = header("s", 5, None);
        let empty = summarize(&h, &[]);
        assert_eq!(empty.event_count, 0);
        assert_eq!(empty.last_event_type, None);
        assert_eq!(empty.last_activity, None);
        assert!(empty.idle, "empty log rests on a boundary");
        let midturn = summarize(
            &h,
            &[
                text_event(0, "turn/start", "go"),
                text_event(1, "user/message", "?"),
            ],
        );
        assert!(!midturn.idle, "open turn is not idle");
        assert_eq!(midturn.last_event_type.as_deref(), Some("user/message"));
    }

    /// WI-107: full-text search hits the matching session with a windowed
    /// snippet and occurrence count; misses stay out; limit truncates.
    #[tokio::test]
    async fn search_sessions_returns_snippet_and_counts() {
        let dir = tempfile::tempdir().unwrap();
        let store = jsonl::JsonlSessionPersistence::open(dir.path().into()).unwrap();
        seed_two_logs(&store).await;

        let hits = search_sessions(&store, "zstd", 20, None).await.unwrap();
        assert_eq!(hits.len(), 1, "only the matching session hits");
        assert_eq!(hits[0].id.0, "s1");
        assert!(hits[0].snippet.contains("zstd"), "snippet carries the needle");
        assert!(hits[0].snippet.starts_with('…') || hits[0].snippet.len() <= 96 + 2);
        assert_eq!(hits[0].matches, 2, "both occurrences counted");

        // Miss: nothing.
        assert!(search_sessions(&store, "quantum", 20, None).await.unwrap().is_empty());
        // Limit 0 / empty needle: nothing, no traversal.
        assert!(search_sessions(&store, "", 20, None).await.unwrap().is_empty());
        assert!(search_sessions(&store, "zstd", 0, None).await.unwrap().is_empty());
    }

    /// Case-insensitivity matches the text-filter semantics (WI-24).
    #[tokio::test]
    async fn search_sessions_is_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let store = jsonl::JsonlSessionPersistence::open(dir.path().into()).unwrap();
        seed_two_logs(&store).await;
        let hits = search_sessions(&store, "ZSTD", 20, None).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].matches, 2);
    }

    async fn seed_two_logs(store: &jsonl::JsonlSessionPersistence) {
        for (id, events) in [
            ("s1", sample_log()),
            ("s2", vec![text_event(0, "user/message", "unrelated question")]),
        ] {
            store
                .append(SessionId(id.into()), events, None)
                .await
                .unwrap();
        }
    }
}
