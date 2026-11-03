//! Browser binary discovery: explicit settings path > `STARRIVER_BROWSER_BIN`
//! env > platform default candidates (Chrome, Chromium, Edge — in that
//! order). Returns the first candidate that exists, so `browser_start` fails
//! with an actionable message instead of a CDP launch error.

use std::path::{Path, PathBuf};

/// Ordered per-platform default candidates.
pub fn platform_candidates() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    } else if cfg!(target_os = "windows") {
        let mut candidates = Vec::new();
        for program_files in ["C:\\Program Files", "C:\\Program Files (x86)"] {
            candidates
                .push(PathBuf::from(program_files).join("Google\\Chrome\\Application\\chrome.exe"));
            candidates.push(
                PathBuf::from(program_files).join("Microsoft\\Edge\\Application\\msedge.exe"),
            );
        }
        candidates
    } else {
        [
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    }
}

/// Whether a path points at an existing file (PATH entries on unix are
/// resolved through `which`-style probing here, without executing anything).
fn exists_executable(path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return false;
    }
    if path.components().count() > 1 {
        return path.is_file();
    }
    // Bare command name: search PATH (unix only; Windows candidates are
    // always absolute).
    if cfg!(unix) {
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path_var) {
                if dir.join(path).is_file() {
                    return true;
                }
            }
        }
    }
    false
}

/// Resolve the browser executable. `explicit` comes from settings
/// (`browser.browser_path`) or the per-call `browserPath` tool parameter.
pub fn discover_browser(explicit: Option<&str>) -> Result<PathBuf, String> {
    if let Some(explicit) = explicit.filter(|s| !s.trim().is_empty()) {
        let path = PathBuf::from(explicit);
        if exists_executable(&path) {
            return Ok(path);
        }
        return Err(format!("configured browser `{explicit}` does not exist"));
    }
    if let Ok(env_path) = std::env::var("STARRIVER_BROWSER_BIN") {
        if !env_path.trim().is_empty() {
            let path = PathBuf::from(&env_path);
            if exists_executable(&path) {
                return Ok(path);
            }
            return Err(format!("STARRIVER_BROWSER_BIN `{env_path}` does not exist"));
        }
    }
    for candidate in platform_candidates() {
        if exists_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(
        "no browser found — install Google Chrome/Chromium/Edge, or set \
         `browser.browser_path` in ~/.starriver/settings.yaml (or the \
         STARRIVER_BROWSER_BIN env var)"
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_missing_path_errors_with_the_path() {
        let err = discover_browser(Some("/nonexistent-browser-xyz")).unwrap_err();
        assert!(err.contains("/nonexistent-browser-xyz"), "{err}");
    }

    #[test]
    fn empty_explicit_falls_through_to_discovery() {
        // On CI-like hosts discovery may fail; the contract is only that an
        // EMPTY explicit path never short-circuits with a missing-file error.
        match discover_browser(Some("  ")) {
            Ok(_) => {}
            Err(e) => assert!(!e.contains("  "), "{e}"),
        }
    }

    #[test]
    fn platform_candidates_are_ordered_chromium_family_first() {
        let candidates = platform_candidates();
        assert!(!candidates.is_empty());
        if cfg!(target_os = "macos") {
            assert!(candidates[0]
                .to_string_lossy()
                .contains("Google Chrome.app"));
        }
    }
}
