// History source for the upstream harness's session store.
//
// Layout: `~/.dsh/sessions/<munged-cwd>/session-<uuid>/session.jsonl.zstd`.
// The first event (`type: "session"`) carries the real cwd; `user/message` and
// `assistant/message` events carry the turns (content block arrays where only
// `text` blocks are kept — `reasoning` blocks are the red line).

use std::io::Read;
use std::path::{Path, PathBuf};

use super::{
    content_text, dig, walk_jsonl, DiscoveredSession, HistorySource, NormalizedTranscript,
};
use crate::chat::{ChatBlock, ChatMessage, ChatRole};

pub struct DshSource {
    root: PathBuf,
}

impl DshSource {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".dsh").join("sessions"),
        }
    }
}

impl HistorySource for DshSource {
    fn id(&self) -> &str {
        "dsh"
    }

    fn discover(&self) -> Vec<DiscoveredSession> {
        let mut files = Vec::new();
        walk_jsonl(&self.root, "zstd", &mut files);
        files
            .into_iter()
            .map(|path| {
                let session_id = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|s| s.to_str())
                    .map(str::to_string);
                DiscoveredSession {
                    project_cwd: read_session_cwd(&path),
                    mtime_millis: super::mtime_millis(&path),
                    session_id,
                    path,
                }
            })
            .collect()
    }

    fn read(&self, session: &DiscoveredSession) -> anyhow::Result<NormalizedTranscript> {
        let raw = std::fs::read(&session.path)?;
        let mut decoder = zstd::Decoder::new(&raw[..])?;
        let mut text = String::new();
        decoder.read_to_string(&mut text)?;
        let mut turns = Vec::new();
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let role = match v.get("type").and_then(|t| t.as_str()) {
                Some("user/message") => ChatRole::User,
                Some("assistant/message") => ChatRole::Assistant,
                _ => continue,
            };
            let content = if role == ChatRole::User {
                v.get("data").and_then(|d| d.get("content"))
            } else {
                dig(&v, "data.message.content")
            };
            let text = content.map(content_text).unwrap_or_default();
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

fn read_session_cwd(path: &Path) -> Option<PathBuf> {
    let raw = std::fs::read(path).ok()?;
    let mut decoder = zstd::Decoder::new(&raw[..]).ok()?;
    let mut text = String::new();
    decoder.read_to_string(&mut text).ok()?;
    for line in text.lines().take(3) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("session") {
            if let Some(cwd) = v.get("cwd").and_then(|c| c.as_str()) {
                return Some(PathBuf::from(cwd));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(root: &Path, dir_name: &str) -> PathBuf {
        let sess_dir = root.join(dir_name).join("session-abc");
        std::fs::create_dir_all(&sess_dir).unwrap();
        let plain = concat!(
            r#"{"type":"session","id":"session-abc","cwd":"/tmp/dshproj"}"#,
            "\n",
            r#"{"type":"permission/preset","seq":0,"data":{"preset":"workspace-write"}}"#,
            "\n",
            r#"{"type":"user/message","seq":8,"data":{"content":[{"type":"text","text":"建一个 agent"}],"role":"user"}}"#,
            "\n",
            r#"{"type":"assistant/message","seq":210,"data":{"turn":1,"message":{"role":"assistant","content":[{"type":"reasoning","text":"SECRET internal"},{"type":"text","text":"done"}]}}}"#,
            "\n",
            r#"{"type":"tool/result","seq":300,"data":{"ok":true}}"#,
            "\n",
        );
        let path = sess_dir.join("session.jsonl.zstd");
        let mut encoder = zstd::Encoder::new(std::fs::File::create(&path).unwrap(), 3).unwrap();
        std::io::Write::write_all(&mut encoder, plain.as_bytes()).unwrap();
        encoder.finish().unwrap();
        path
    }

    #[test]
    fn discovers_and_parses_zstd_log() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_fixture(&dir.path().join(".dsh").join("sessions"), "--tmp-dshproj--");
        let src = DshSource::new(dir.path());
        let found = src.discover();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].project_cwd, Some(PathBuf::from("/tmp/dshproj")));
        assert_eq!(found[0].session_id.as_deref(), Some("session-abc"));
        let t = src.read(&found[0]).unwrap();
        assert_eq!(t.turns.len(), 2);
        let text = crate::extract::extractable_text(&t.turns);
        assert!(text.contains("建一个 agent"));
        assert!(text.contains("done"));
        assert!(
            !text.contains("SECRET"),
            "reasoning blocks are the red line"
        );
        let _ = p;
    }

    #[test]
    fn corrupt_zstd_reports_error_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let sess_dir = dir
            .path()
            .join(".dsh")
            .join("sessions")
            .join("--x--")
            .join("session-bad");
        std::fs::create_dir_all(&sess_dir).unwrap();
        std::fs::write(sess_dir.join("session.jsonl.zstd"), b"garbage").unwrap();
        let src = DshSource::new(dir.path());
        let found = src.discover();
        assert!(src.read(&found[0]).is_err());
    }
}
