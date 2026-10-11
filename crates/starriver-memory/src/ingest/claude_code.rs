// history source.
//
// Layout: `~/.claude/projects/<encoded-cwd>/*.jsonl`. Each line is one event;
// `type: "user" | "assistant"` lines carry `message.role` + `message.content`
// (plain string or block array). Sidechain (subagent) entries are skipped.
//
// The encoded directory name is Claude's own munging (`/` → `-`); decoding is
// heuristic and ambiguous for path components that genuinely contain `-`. We
// validate the decode against the filesystem and fall back to `_global` when
// it doesn't resolve.

use std::path::{Path, PathBuf};

use super::{content_text, walk_jsonl, DiscoveredSession, HistorySource, NormalizedTranscript};
use crate::chat::{ChatBlock, ChatMessage, ChatRole};

pub struct ClaudeCodeSource {
    root: PathBuf,
}

impl ClaudeCodeSource {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".claude").join("projects"),
        }
    }
}

impl HistorySource for ClaudeCodeSource {
    fn id(&self) -> &str {
        "claude-code"
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
                    .map(str::to_string);
                DiscoveredSession {
                    project_cwd: path.parent().and_then(decode_project_dir),
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
            if v.get("isSidechain").and_then(|s| s.as_bool()) == Some(true) {
                continue;
            }
            let role = match v.get("type").and_then(|t| t.as_str()) {
                Some("user") => ChatRole::User,
                Some("assistant") => ChatRole::Assistant,
                _ => continue,
            };
            // Local-command wrappers (<command-name> etc.) are UI noise, not
            // conversation.
            let text = v
                .get("message")
                .and_then(|m| m.get("content"))
                .map(content_text)
                .unwrap_or_default();
            // Only drop the local-command WRAPPERS (<command-name>,
            // <local-command-stdout> etc.) — NOT every turn that happens to
            // start with `<` (real user turns quoting HTML/JSX were being
            // silently discarded).
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            let is_wrapper = trimmed.starts_with("<command-")
                || trimmed.starts_with("<local-command")
                || trimmed.starts_with("<system-reminder>");
            if is_wrapper {
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

/// Decode a Claude project directory name (`-Users-x-foo-bar`) back to a path,
/// accepting it only when the directory exists on this machine.
fn decode_project_dir(dir: &Path) -> Option<PathBuf> {
    let name = dir.file_name()?.to_str()?;
    if !name.starts_with('-') {
        return None;
    }
    let decoded = PathBuf::from(name.replacen('-', "/", 1).replace('-', "/"));
    decoded.exists().then_some(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &Path, name: &str) {
        std::fs::write(
            dir.join(format!("{name}.jsonl")),
            concat!(
                r#"{"type":"mode","mode":"normal"}"#, "\n",
                r#"{"type":"user","isSidechain":false,"message":{"role":"user","content":"项目用 pnpm"}}"#, "\n",
                r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"好的"}]}}"#, "\n",
                r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"sidechain noise"}}"#, "\n",
                r#"{"type":"user","message":{"role":"user","content":"<local-command-caveat>caveat</local-command-caveat"}}"#, "\n",
            ),
        )
        .unwrap();
    }

    #[test]
    fn parses_user_and_assistant_dropping_noise() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let real = PathBuf::from("/tmp"); // exists → decode accepted
        let proj_dir = dir.path().join(".claude").join("projects").join("-tmp");
        std::fs::create_dir_all(&proj_dir).unwrap();
        fixture(&proj_dir, "s1");
        let src = ClaudeCodeSource::new(dir.path());
        let found = src.discover();
        assert_eq!(found.len(), 1);
        // `/tmp` only exists as an absolute path on Unix; on Windows the
        // decoded cwd fails the `exists` check and is (correctly) dropped.
        #[cfg(unix)]
        assert_eq!(found[0].project_cwd, Some(real.clone()));
        #[cfg(windows)]
        assert_eq!(found[0].project_cwd, None);
        let t = src.read(&found[0]).unwrap();
        assert_eq!(t.turns.len(), 2);
        let text = crate::extract::extractable_text(&t.turns);
        assert!(text.contains("项目用 pnpm"));
        assert!(!text.contains("sidechain"), "sidechain skipped");
        assert!(!text.contains("local-command"), "command wrapper skipped");
    }

    #[test]
    fn undecodable_project_dir_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        let proj_dir = dir
            .path()
            .join(".claude")
            .join("projects")
            .join("-no-such-path");
        std::fs::create_dir_all(&proj_dir).unwrap();
        fixture(&proj_dir, "s2");
        let src = ClaudeCodeSource::new(dir.path());
        let found = src.discover();
        assert_eq!(
            found[0].project_cwd, None,
            "unresolvable cwd → _global later"
        );
    }
}
