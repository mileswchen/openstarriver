// history source.
//
// Layout: `~/./cli/rollout/model-io-sess_*.jsonl`. Each record is one
// model round-trip: `request.body.messages` holds the conversation-so-far
// (the LAST user message is this round's new input) and `response.text` is the
// assistant output. There is no cwd in the record — transcripts land in
// `_global` unless a future format adds one.

use std::path::{Path, PathBuf};

use super::{content_text, walk_jsonl, DiscoveredSession, HistorySource, NormalizedTranscript};
use crate::chat::{ChatBlock, ChatMessage, ChatRole};

pub struct ZcodeSource {
    root: PathBuf,
}

impl ZcodeSource {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".zcode").join("cli").join("rollout"),
        }
    }
}

impl HistorySource for ZcodeSource {
    fn id(&self) -> &str {
        "zcode"
    }

    fn discover(&self) -> Vec<DiscoveredSession> {
        let mut files = Vec::new();
        walk_jsonl(&self.root, "jsonl", &mut files);
        files
            .into_iter()
            .map(|path| {
                let session_id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.strip_prefix("model-io-").map(str::to_string));
                DiscoveredSession {
                    project_cwd: None,
                    mtime_millis: super::mtime_millis(&path),
                    session_id,
                    path,
                }
            })
            .collect()
    }

    fn read(&self, session: &DiscoveredSession) -> anyhow::Result<NormalizedTranscript> {
        let mut turns = Vec::new();
        for line in super::read_json_lines(&session.path)? {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            // The new user input of this round = the last user message in the
            // request (earlier ones were already emitted by previous records).
            let last_user = v
                .get("request")
                .and_then(|r| r.get("body"))
                .and_then(|b| b.get("messages"))
                .and_then(|ms| ms.as_array())
                .and_then(|ms| {
                    ms.iter()
                        .rev()
                        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                })
                .and_then(|m| m.get("content"))
                .map(content_text)
                .unwrap_or_default();
            if !last_user.trim().is_empty() {
                turns.push(ChatMessage {
                    role: ChatRole::User,
                    content: vec![ChatBlock::Text { text: last_user }],
                });
            }
            let assistant = v
                .get("response")
                .and_then(|r| r.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            if !assistant.trim().is_empty() {
                turns.push(ChatMessage {
                    role: ChatRole::Assistant,
                    content: vec![ChatBlock::Text { text: assistant }],
                });
            }
        }
        Ok(NormalizedTranscript {
            source_agent: self.id().to_string(),
            session_id: session.session_id.clone(),
            project_cwd: session.project_cwd.clone(),
            turns,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_round_trips_deduplicating_history() {
        let dir = tempfile::tempdir().unwrap();
        let roll = dir.path().join(".zcode").join("cli").join("rollout");
        std::fs::create_dir_all(&roll).unwrap();
        std::fs::write(
            roll.join("model-io-sess_abc.jsonl"),
            concat!(
                r#"{"sessionId":"abc","request":{"body":{"messages":[{"role":"system","content":"sys"},{"role":"user","content":"first question"}]}},"response":{"text":"first answer"}}"#, "\n",
                r#"{"sessionId":"abc","request":{"body":{"messages":[{"role":"system","content":"sys"},{"role":"user","content":"first question"},{"role":"assistant","content":"first answer"},{"role":"user","content":"second question"}]}},"response":{"text":"second answer"}}"#, "\n",
            ),
        )
        .unwrap();
        let src = ZcodeSource::new(dir.path());
        let found = src.discover();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id.as_deref(), Some("sess_abc"));
        let t = src.read(&found[0]).unwrap();
        let text = crate::extract::extractable_text(&t.turns);
        assert_eq!(
            text.matches("first question").count(),
            1,
            "old turns not re-emitted"
        );
        assert!(text.contains("second question"));
        assert!(text.contains("second answer"));
        assert!(!text.contains("sys"), "system prompts skipped");
        assert_eq!(t.turns.len(), 4);
    }
}
