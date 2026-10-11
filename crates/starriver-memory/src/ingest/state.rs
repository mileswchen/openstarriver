// Incremental ingestion watermark: `{source_id: {file_path: mtime_millis}}`,
// persisted as JSON next to the memory store. A session is re-processed only
// when its path is new or its mtime moved past the recorded value.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct IngestState {
    path: Option<PathBuf>,
    inner: HashMap<String, HashMap<String, u64>>,
}

impl IngestState {
    /// State that is never persisted (tests, one-shot sweeps).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Load from `path`; a missing or corrupt file starts fresh (ingestion is
    /// idempotent, so losing the watermark only costs one re-scan).
    pub fn load(path: &Path) -> Self {
        let inner = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path: Some(path.to_path_buf()),
            inner,
        }
    }

    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(&self.inner) {
            let _ = std::fs::write(path, text);
        }
    }

    /// True when this file has not been seen at (or after) this mtime.
    pub fn is_new(&self, source_id: &str, path: &Path, mtime_millis: u64) -> bool {
        self.inner
            .get(source_id)
            .and_then(|files| files.get(path.to_string_lossy().as_ref()))
            .map(|seen| *seen < mtime_millis)
            .unwrap_or(true)
    }

    pub fn mark(&mut self, source_id: &str, path: &Path, mtime_millis: u64) {
        self.inner
            .entry(source_id.to_string())
            .or_default()
            .insert(path.to_string_lossy().into_owned(), mtime_millis);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watermark_advances_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ingest_state.json");
        let mut st = IngestState::load(&path);
        let f = dir.path().join("a.jsonl");
        assert!(st.is_new("codex", &f, 100));
        st.mark("codex", &f, 100);
        assert!(!st.is_new("codex", &f, 100));
        assert!(st.is_new("codex", &f, 200), "newer mtime re-processes");
        assert!(st.is_new("dsh", &f, 100), "sources tracked separately");
        st.save();

        let re = IngestState::load(&path);
        assert!(!re.is_new("codex", &f, 100));
    }

    #[test]
    fn missing_or_corrupt_file_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert!(IngestState::load(&path).is_new("x", &path, 1));
        std::fs::write(&path, "not json").unwrap();
        assert!(IngestState::load(&path).is_new("x", &path, 1));
    }
}
