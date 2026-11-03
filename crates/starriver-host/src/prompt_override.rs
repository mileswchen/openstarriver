//! Runtime prompt-override seam: let files replace the compiled-in
//! built-in system-prompt sections without recompiling.
//!
//! Contract: `STARRIVER_PROMPT_OVERRIDE_DIR` points at a directory of
//! per-section markdown files. A present, non-empty file replaces the
//! built-in default text for that section; a missing file keeps the
//! default; a settings opt-out (e.g. `include_identity=false`) still wins
//! over any override. Sections whose presence is settings-driven
//! (`harness:source`, `deployment:baseline`) only take an override when the
//! section renders at all.
//!
//! Override file stem → section mapping:
//!   `identity.md`       → `identity` (-100)
//!   `source.md`         → `harness:source` (-99, only when enabled)
//!   `baseline.md`       → `deployment:baseline` (0, only when configured)
//!   `language.md`       → `language` (10)
//!   `communication.md`  → `communication` (20)
//!
//! Intended consumers: harness adapters that stage component trees (e.g. a
//! Harness² substrate materialising its `systemprompt.md` component) and
//! power users swapping prompt packages. The directory is read per
//! assembly (no cache) so staged edits land on the next run without a
//! restart.

use std::path::{Path, PathBuf};

/// Env var naming the override directory.
pub const OVERRIDE_DIR_ENV: &str = "STARRIVER_PROMPT_OVERRIDE_DIR";

/// Single override file read cap (anti-misconfig guard, mirrors the
/// workspace-instructions engine's per-file cap).
const MAX_OVERRIDE_BYTES: u64 = 1 << 20;

/// The override directory from the environment, if set and non-empty.
pub fn override_dir() -> Option<PathBuf> {
    std::env::var(OVERRIDE_DIR_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Override text for one section from an explicit directory (testable core).
/// `None` = no override: file missing, unreadable, oversized, or empty.
pub fn section_override_in(dir: &Path, stem: &str) -> Option<String> {
    let path = dir.join(format!("{stem}.md"));
    let meta = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        // Absent override is the normal case — stay quiet.
        Err(_) => return None,
    };
    if !meta.is_file() {
        return None;
    }
    if meta.len() > MAX_OVERRIDE_BYTES {
        tracing::warn!(
            path = %path.display(),
            size = meta.len(),
            "prompt override exceeds the 1 MiB cap; ignored"
        );
        return None;
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "failed to read prompt override; using built-in default"
            );
            None
        }
    }
}

/// Override text for one section, resolving the directory from the environment.
pub fn section_override(stem: &str) -> Option<String> {
    section_override_in(&override_dir()?, stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(stem: &str, content: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(format!("{stem}.md")), content).unwrap();
        dir
    }

    #[test]
    fn missing_file_is_no_override() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(section_override_in(dir.path(), "identity"), None);
    }

    #[test]
    fn present_file_overrides_and_trims() {
        let dir = dir_with("identity", "  custom identity  \n");
        assert_eq!(
            section_override_in(dir.path(), "identity"),
            Some("custom identity".into())
        );
    }

    #[test]
    fn empty_file_is_no_override() {
        let dir = dir_with("communication", "   \n");
        assert_eq!(section_override_in(dir.path(), "communication"), None);
    }

    #[test]
    fn oversized_file_is_ignored() {
        let dir = dir_with("identity", &"x".repeat((MAX_OVERRIDE_BYTES + 1) as usize));
        assert_eq!(section_override_in(dir.path(), "identity"), None);
    }

    #[test]
    fn subdirectory_entry_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("identity.md")).unwrap();
        assert_eq!(section_override_in(dir.path(), "identity"), None);
    }
}
