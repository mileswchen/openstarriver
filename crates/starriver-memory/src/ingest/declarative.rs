// Declarative history sources (zero-code agent onboarding).
//
// `~/.starriver/memory/sources.yaml` lets a user describe a new agent's local
// history store (anything JSON-lines/array shaped) without
// writing Rust:
//
// ```yaml
// sources:
//   other-agent:
//     glob: "~/.other-agent/sessions/**/*.jsonl"
//     cwd_fields: ["meta.cwd", "cwd", "workspace"]
//     user_fields: ["payload.text", "text"]
//     assistant_fields: ["payload.reply", "reply"]
//   another-agent:
//     glob: "~/Library/Application Support/AnotherAgent/*/sessions/*.json"
//     format: json-array
//     cwd_fields: ["workspaceFolder"]
//     user_fields: ["message"]
//     assistant_fields: ["response"]
// ```
//
// Each line/element that yields text under a `user_fields` path becomes a user
// turn; likewise for assistant. Everything else is dropped (red line).

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{dig, DiscoveredSession, HistorySource, NormalizedTranscript};
use crate::chat::{ChatBlock, ChatMessage, ChatRole};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeclarativeFormat {
    /// One JSON object per line (the common case).
    JsonlEvents,
    /// One file = one JSON array of objects.
    JsonArray,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeclarativeSourceConfig {
    /// Glob-ish root path; only the leading `~` and `**` recursion into `*.ext`
    /// are supported (see `resolve_glob`). Defaults to enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub glob: String,
    #[serde(default = "default_format")]
    pub format: DeclarativeFormat,
    #[serde(default)]
    pub cwd_fields: Vec<String>,
    #[serde(default)]
    pub user_fields: Vec<String>,
    #[serde(default)]
    pub assistant_fields: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn default_format() -> DeclarativeFormat {
    DeclarativeFormat::JsonlEvents
}

#[derive(Debug, Default, Deserialize)]
pub struct DeclarativeSourcesFile {
    #[serde(default)]
    pub sources: std::collections::BTreeMap<String, DeclarativeSourceConfig>,
}

impl DeclarativeSourceConfig {
    pub fn build(&self, id: &str) -> DeclarativeSource {
        DeclarativeSource {
            id: id.to_string(),
            config: self.clone(),
            files: resolve_glob(&self.glob),
        }
    }
}

/// Parse a sources.yaml document into concrete sources. Unknown/invalid
/// entries are skipped with a warning, never fatal.
pub fn declarative_sources_from_yaml(yaml: &str) -> Vec<DeclarativeSource> {
    let Ok(file) = serde_yaml::from_str::<DeclarativeSourcesFile>(yaml) else {
        tracing::warn!("sources.yaml is invalid — declarative sources disabled");
        return Vec::new();
    };
    file.sources
        .iter()
        .filter(|(_, cfg)| cfg.enabled)
        .map(|(id, cfg)| cfg.build(id))
        .collect()
}

pub struct DeclarativeSource {
    id: String,
    config: DeclarativeSourceConfig,
    files: Vec<PathBuf>,
}

impl HistorySource for DeclarativeSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn discover(&self) -> Vec<DiscoveredSession> {
        self.files
            .iter()
            .map(|path| DiscoveredSession {
                project_cwd: None,
                mtime_millis: super::mtime_millis(path),
                session_id: path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string),
                path: path.clone(),
            })
            .collect()
    }

    fn read(&self, session: &DiscoveredSession) -> anyhow::Result<NormalizedTranscript> {
        let mut turns = Vec::new();
        let mut cwd: Option<PathBuf> = None;
        let feed =
            |v: &serde_json::Value, turns: &mut Vec<ChatMessage>, cwd: &mut Option<PathBuf>| {
                if cwd.is_none() {
                    for field in &self.config.cwd_fields {
                        if let Some(c) = dig(v, field).and_then(|c| c.as_str()) {
                            if !c.is_empty() {
                                *cwd = Some(PathBuf::from(c));
                                break;
                            }
                        }
                    }
                }
                for (fields, role) in [
                    (&self.config.user_fields, ChatRole::User),
                    (&self.config.assistant_fields, ChatRole::Assistant),
                ] {
                    for field in fields {
                        if let Some(text) = dig(v, field).and_then(|t| t.as_str()) {
                            if !text.trim().is_empty() {
                                turns.push(ChatMessage {
                                    role,
                                    content: vec![ChatBlock::Text {
                                        text: text.to_string(),
                                    }],
                                });
                                break;
                            }
                        }
                    }
                }
            };
        match self.config.format {
            DeclarativeFormat::JsonlEvents => {
                for line in super::read_json_lines(&session.path)? {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                        feed(&v, &mut turns, &mut cwd);
                    }
                }
            }
            DeclarativeFormat::JsonArray => {
                let text = std::fs::read_to_string(&session.path)?;
                if let Ok(serde_json::Value::Array(items)) = serde_json::from_str(&text) {
                    for v in &items {
                        feed(v, &mut turns, &mut cwd);
                    }
                }
            }
        }
        Ok(NormalizedTranscript {
            source_agent: self.id.clone(),
            session_id: session.session_id.clone(),
            project_cwd: cwd.or_else(|| session.project_cwd.clone()),
            turns,
        })
    }
}

/// Expand `~` and wildcards. The literal prefix (components before the first
/// wildcard) is walked recursively; the last component fixes the extension
/// filter when it looks like `*.ext` (a bare `**` matches every file).
fn resolve_glob(glob: &str) -> Vec<PathBuf> {
    let expanded = if let Some(rest) = glob.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) => Path::new(&home).join(rest),
            Err(_) => return Vec::new(),
        }
    } else {
        PathBuf::from(glob)
    };
    let components: Vec<String> = expanded
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let wildcard_at = components.iter().position(|c| c.contains('*'));
    let Some(at) = wildcard_at else {
        return expanded.is_file().then_some(expanded).into_iter().collect();
    };
    let mut root = PathBuf::new();
    for c in &components[..at] {
        root.push(c);
    }
    // Extension filter from the LAST component when it is `*.ext` / `**.ext`.
    let ext = components
        .last()
        .and_then(|c| c.strip_prefix("*."))
        .or_else(|| components.last().and_then(|c| c.strip_prefix("**.")))
        .map(str::to_string);
    let mut out = Vec::new();
    collect_all_files(&root, &mut out);
    if let Some(ext) = ext {
        out.retain(|p| p.extension().and_then(|e| e.to_str()) == Some(ext.as_str()));
    }
    out
}

fn collect_all_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_all_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml() -> &'static str {
        r#"
sources:
  qodar:
    glob: "~/.qodar-not-real/sessions/**/*.jsonl"
    cwd_fields: ["meta.cwd"]
    user_fields: ["payload.text"]
    assistant_fields: ["payload.reply"]
  disabled-one:
    enabled: false
    glob: "~/.nope/*.jsonl"
"#
    }

    #[test]
    fn parses_and_skips_disabled() {
        let sources = declarative_sources_from_yaml(yaml());
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].id, "qodar");
    }

    #[test]
    fn invalid_yaml_is_empty_not_panic() {
        assert!(declarative_sources_from_yaml(": : :").is_empty());
    }

    #[test]
    fn jsonl_events_source_reads_user_assistant_and_cwd() {
        // Point the glob at a real temp dir via HOME override.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sessions").join("2026");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("s1.jsonl"),
            concat!(
                r#"{"meta":{"cwd":"/tmp/qodarproj"}}"#,
                "\n",
                r#"{"payload":{"text":"用 pnpm"}}"#,
                "\n",
                r#"{"payload":{"reply":"ok"}}"#,
                "\n",
                r#"{"other":"noise"}}"#,
                "\n",
            ),
        )
        .unwrap();
        // Forward slashes: temp path lands inside a YAML double-quoted
        // scalar, where Windows backslashes would read as escape sequences.
        let glob_root = dir.path().display().to_string().replace('\\', "/");
        let cfg: DeclarativeSourcesFile = serde_yaml::from_str(&format!(
            "sources:\n  qodar:\n    glob: \"{glob_root}/sessions/**/*.jsonl\"\n    cwd_fields: [\"meta.cwd\"]\n    user_fields: [\"payload.text\"]\n    assistant_fields: [\"payload.reply\"]\n"
        ))
        .unwrap();
        let src = cfg.sources["qodar"].build("qodar");
        let found = src.discover();
        assert_eq!(found.len(), 1);
        let t = src.read(&found[0]).unwrap();
        assert_eq!(t.source_agent, "qodar");
        assert_eq!(t.turns.len(), 2);
        assert_eq!(t.project_cwd, Some(PathBuf::from("/tmp/qodarproj")));
        assert_eq!(t.turns[0].role, ChatRole::User);
    }

    #[test]
    fn json_array_source_reads_elements() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("s1.json"),
            r#"[{"workspaceFolder":"/tmp/traeproj","message":"hi"},{"response":"hello"}]"#,
        )
        .unwrap();
        // Forward slashes: temp path lands inside a YAML double-quoted
        // scalar, where Windows backslashes would read as escape sequences.
        let glob_root = dir.path().display().to_string().replace('\\', "/");
        let cfg: DeclarativeSourcesFile = serde_yaml::from_str(&format!(
            "sources:\n  trae:\n    glob: \"{glob_root}/*.json\"\n    format: json-array\n    cwd_fields: [\"workspaceFolder\"]\n    user_fields: [\"message\"]\n    assistant_fields: [\"response\"]\n"
        ))
        .unwrap();
        let src = cfg.sources["trae"].build("trae");
        let found = src.discover();
        assert_eq!(found.len(), 1);
        let t = src.read(&found[0]).unwrap();
        assert_eq!(t.turns.len(), 2);
        assert_eq!(t.project_cwd, Some(PathBuf::from("/tmp/traeproj")));
    }

    #[test]
    fn resolve_glob_missing_root_is_empty() {
        assert!(resolve_glob("~/definitely-not-here-12345/**/*.jsonl").is_empty());
    }
}
