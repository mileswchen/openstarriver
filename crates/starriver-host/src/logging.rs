//! Host-process tracing (WI-163): every `tracing::warn!`/`error!` in the
//! host and MCP bridge lands in `~/.starriver/logs/host.log` with daily
//! rotation. Before this the packaged app never installed a subscriber —
//! MCP discovery failures were dropped silently, which is why the 2026-10-01
//! nexplan outage could only be reconstructed forensically from the session
//! store. `RUST_LOG` overrides the default `info` filter.

use std::path::PathBuf;

/// The subscriber `init_host_tracing` installs: WARN+ to a daily-rotated
/// `host.log` under `dir`, filter from `RUST_LOG` (default `info`).
/// Extracted so tests can exercise the exact production write path against
/// a directory they own (the global appender's location depends on which
/// thread wins the init race, and sibling tests swap `HOME`).
fn build_file_subscriber(dir: &std::path::Path) -> impl tracing::Subscriber + Send + Sync + 'static {
    let appender = tracing_appender::rolling::daily(dir, "host.log");
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(appender)
        .with_ansi(false)
        .finish()
}

/// Install the global file subscriber. Idempotent: later calls (extra
/// `HostRuntime::new` in the same process, tests) are no-ops.
pub fn init_host_tracing() {
    use std::sync::OnceLock;
    static INIT: OnceLock<()> = OnceLock::new();
    if INIT.set(()).is_err() {
        return;
    }
    let Some(dir) = log_dir() else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = tracing::subscriber::set_global_default(build_file_subscriber(&dir));
}

/// `~/.starriver/logs`, or None when HOME is unavailable.
pub fn log_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".starriver").join("logs"))
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Scoped tracing capture for the host test binary: `with_default`
    //! installs a thread-local subscriber for the closure's duration, so
    //! warn assertions work even when another test already set the global
    //! (file) subscriber.

    #[derive(Clone)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Run `f` under a capturing subscriber and return everything it logged
    /// (WARN and above).
    pub fn capture_tracing<F: FnOnce()>(f: F) -> String {
        let sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = std::sync::Arc::clone(&sink);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || SharedBuf(writer.clone()))
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = sink.lock().unwrap().clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent_and_never_panics() {
        // Twice in one process: the OnceLock plus try_init must both hold.
        init_host_tracing();
        init_host_tracing();
    }

    #[test]
    fn log_dir_sits_under_the_starriver_root() {
        let dir = log_dir().expect("HOME is set in tests");
        assert!(dir.ends_with(".starriver/logs"), "{}", dir.display());
    }

    /// WI-164: end-to-end over the exact production write path — a warn
    /// emitted under the file subscriber must physically land in the day's
    /// file. Hermetic (own tempdir + scoped subscriber): the global
    /// appender's location depends on the init race, and sibling tests swap
    /// `HOME`, so pinning the real dir would be flaky.
    #[test]
    fn warns_reach_the_daily_file() {
        let dir = tempfile::tempdir().unwrap();
        let subscriber = build_file_subscriber(dir.path());
        let marker = format!(
            "wi164-warn-reaches-file-{}",
            std::process::id() as u64 + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(marker = %marker, "smoke");
        });
        let written = std::fs::read_dir(dir.path())
            .expect("log dir exists")
            .flatten()
            .find(|entry| entry.file_name().to_string_lossy().starts_with("host.log."))
            .map(|entry| std::fs::read_to_string(entry.path()).expect("log file readable"))
            .expect("the daily log file was created");
        assert!(
            written.contains(&marker),
            "smoke warn must land in the daily file: {written}"
        );
    }
}
