//! Shared test helpers for the host crate's unit tests.
//!
//! `with_temp_home` is the only sanctioned way to mutate `$HOME` in host tests:
//! `std::env::set_var` is process-global, so parallel test threads that each
//! swap `$HOME` race — one thread's write can land in whatever dir another
//! thread is currently pointing at, silently redirecting file IO. Every host
//! HOME-mutating test must go through the shared [`HOME_LOCK`].

#[cfg(test)]
pub(crate) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Point `$HOME` at a throwaway tempdir for the duration of `f`, restoring it
/// afterwards. The tempdir path is passed to `f` so tests can seed files there.
#[cfg(test)]
pub(crate) fn with_temp_home<F: FnOnce(&std::path::Path)>(f: F) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| {
        HOME_LOCK.clear_poison();
        e.into_inner()
    });
    // Keychain round trips stall unsigned test binaries on macOS.
    std::env::set_var("STARRIVER_DISABLE_KEYCHAIN", "1");
    let tmp = tempfile::tempdir().unwrap();
    let old = std::env::var("HOME").ok();
    std::env::set_var("HOME", tmp.path());
    f(tmp.path());
    if let Some(h) = old {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
}

/// A writable directory guaranteed OUTSIDE the workspace sandbox boundary,
/// for `*_outside_workspace_rejected` tests. The system temp dir cannot play
/// this role on Linux: both the workspace and the "outside" tempdirs land
/// under `/tmp`, which workspace-write deliberately whitelists, so the
/// lexical boundary check would rightly allow the write and the premise
/// breaks (macOS never noticed — its tempdirs live in `/var/folders/…`).
/// HOME is always outside the temp dir on dev machines and CI.
#[cfg(test)]
pub(crate) fn outside_boundary_dir(tag: &str) -> std::path::PathBuf {
    // Serialize against `with_temp_home` swappers: computing the path while
    // another test holds HOME at a tempdir would land the "outside" dir
    // inside %TEMP% — inside the boundary — and break the test's premise.
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| {
        HOME_LOCK.clear_poison();
        e.into_inner()
    });
    // Windows shells don't always export HOME; USERPROFILE is the same
    // location there and is equally outside the temp-dir whitelist.
    let home = std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok())
        .expect("boundary tests need a real HOME outside the system temp dir");
    let dir = std::path::Path::new(&home).join(format!(
        ".starriver-sbx-outside-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create outside-boundary dir");
    dir
}
