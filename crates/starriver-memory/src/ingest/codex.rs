// (Desktop & CLI) history source.
//
// Layout: `~/.sessions/YYYY/MM/DD/rollout-*.jsonl`. Each line is an
// event; `session_meta` carries the cwd, `response_item` events with
// `payload.type == "message"` carry the turns. `developer`/`system` roles are
// 's own instruction plumbing and are skipped.

use std::path::{Path, PathBuf};

use super::{
    content_text, dig, walk_jsonl, DiscoveredSession, HistorySource, NormalizedTranscript,
};
use crate::chat::{ChatBlock, ChatMessage, ChatRole};

pub struct CodexSource {
    root: PathBuf,
}

impl CodexSource {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".codex").join("sessions"),
        }
    }
}

impl HistorySource for CodexSource {
    fn id(&self) -> &str {
        "codex"
    }

    fn discover(&self) -> Vec<DiscoveredSession> {
        let mut files = Vec::new();
        walk_jsonl(&self.root, "jsonl", &mut files);
        let mut out = Vec::new();
        for path in files {
            let cwd = read_meta_cwd(&path);
            let session_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.rsplit('-').next().map(str::to_string));
            out.push(DiscoveredSession {
                mtime_millis: super::mtime_millis(&path),
                path,
                project_cwd: cwd,
                session_id,
            });
        }
        out
    }

    fn read(&self, session: &DiscoveredSession) -> anyhow::Result<NormalizedTranscript> {
        let mut turns = Vec::new();
        for line in super::read_json_lines(&session.path)? {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
                continue;
            }
            let Some(payload) = v.get("payload") else {
                continue;
            };
            if payload.get("type").and_then(|t| t.as_str()) != Some("message") {
                continue;
            }
            let role = match payload.get("role").and_then(|r| r.as_str()).unwrap_or("") {
                "user" => ChatRole::User,
                "assistant" => ChatRole::Assistant,
                _ => continue, // developer / system are internal plumbing
            };
            let text = payload.get("content").map(content_text).unwrap_or_default();
            if text.trim().is_empty() {
                continue;
            }
            turns.push(ChatMessage {
                role,
                content: vec![ChatBlock::Text { text }],
            });
        }
        Ok(NormalizedTranscript {
            source_agent: self.id().to_string(),
            session_id: session.session_id.clone(),
            project_cwd: session.project_cwd.clone(),
            turns,
        })
    }
}

/// Read the `session_meta.payload.cwd` from the first lines of a rollout file
/// (bounded read — discovery must stay cheap).
fn read_meta_cwd(path: &Path) -> Option<PathBuf> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().map_while(Result::ok).take(5) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("session_meta") {
            if let Some(cwd) = dig(&v, "payload.cwd").and_then(|c| c.as_str()) {
                return Some(PathBuf::from(cwd));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(dir: &Path) -> PathBuf {
        let p = dir.join("rollout-2026-01-01T00-00-00-abc123.jsonl");
        std::fs::write(
            &p,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"abc123","cwd":"/tmp/proj"}}"#, "\n",
                r#"{"type":"token_count","payload":{}}"#, "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"system instructions"}]}}"#, "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"use pnpm here"}]}}"#, "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"got it"}]}}"#, "\n",
                r#"{"type":"response_item","payload":{"type":"function_call","name":"shell"}"#, "\n",
            ),
        )
        .unwrap();
        p
    }

    #[test]
    fn discovers_and_parses_codex_rollout() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir
            .path()
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("01")
            .join("01");
        std::fs::create_dir_all(&sessions).unwrap();
        let p = write_fixture(&sessions);
        let src = CodexSource::new(dir.path());
        let found = src.discover();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].project_cwd, Some(PathBuf::from("/tmp/proj")));
        assert_eq!(found[0].session_id.as_deref(), Some("abc123"));
        let t = src.read(&found[0]).unwrap();
        assert_eq!(t.source_agent, "codex");
        assert_eq!(t.turns.len(), 2);
        assert_eq!(t.turns[0].role, ChatRole::User);
        let text = crate::extract::extractable_text(&t.turns);
        assert!(text.contains("use pnpm here"));
        assert!(
            !text.contains("system instructions"),
            "developer role skipped"
        );
        let _ = p;
    }

    #[test]
    fn missing_root_discovers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let src = CodexSource::new(dir.path());
        assert!(src.discover().is_empty());
    }
}
