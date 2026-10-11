//! WI-100: cold session log compression — a background best-effort task that
//! zstd-compresses session logs not touched for `SESSION_COLD_DAYS` (default
//! 14). A run marker prevents overlap. Reads are transparent (the jsonl
//! reader decompresses `.jsonl.zst` on demand — decompressed content is
//! never written back to disk, so a cold session stays compressed).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Days before a session log is considered cold.
pub const SESSION_COLD_DAYS: u64 = 14;

/// Marker file preventing overlapping compression runs.
fn marker_path(root: &Path) -> PathBuf {
    root.join(".compress-running")
}

/// Compress every cold `.jsonl` under `<root>/sessions/`. Returns the count
/// compressed. Skips already-compressed (`.zst`), hot (recently modified),
/// and empty files. The marker is best-effort cleanup-on-start.
pub fn compress_cold_sessions(root: &Path) -> usize {
    let sessions_dir = root.join("sessions");
    if !sessions_dir.is_dir() {
        return 0;
    }
    let marker = marker_path(root);
    // BUG-89: create-new (O_EXCL) so a LIVE concurrent run's marker actually
    // stops us; only a stale marker (crashed run, older than an hour) is
    // cleared and re-taken — and losing the re-take race means another run
    // won, which is fine.
    match claim_marker(&marker) {
        true => {}
        false => return 0,
    }
    let count = compress_dir(&sessions_dir);
    let _ = std::fs::remove_file(&marker);
    count
}

/// Try to take the run marker; `false` when a live run holds it.
fn claim_marker(marker: &Path) -> bool {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
    {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let stale = std::fs::metadata(marker)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(3600));
            if !stale {
                return false;
            }
            let _ = std::fs::remove_file(marker);
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(marker)
                .is_ok()
        }
        Err(_) => false,
    }
}

fn compress_dir(dir: &Path) -> usize {
    let mut count = 0;
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.len() < 1024 {
            continue; // Tiny files: compression overhead not worth it.
        }
        let Ok(modified) = meta.modified() else { continue };
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO);
        if age < Duration::from_secs(SESSION_COLD_DAYS * 24 * 3600) {
            continue;
        }
        if compress_one(&path) {
            count += 1;
        }
    }
    count
}

/// Compress a single file: `<name>.jsonl` → `<name>.jsonl.zst`. Returns
/// true on success (original removed only after the .zst lands).
fn compress_one(path: &Path) -> bool {
    let Ok(meta0) = std::fs::metadata(path) else { return false };
    let fingerprint = (meta0.len(), meta0.modified().ok());
    let Ok(bytes) = std::fs::read(path) else { return false };
    let Ok(compressed) = zstd::encode_all(&bytes[..], 3) else { return false };
    let dest = path_with_extension(path, "jsonl.zst");
    let Ok(mut file) = std::fs::File::create(&dest) else { return false };
    use std::io::Write;
    if file.write_all(&compressed).is_err() {
        let _ = std::fs::remove_file(&dest);
        return false;
    }
    // fsync before removing the original: crash between write and unlink
    // leaves both (re-run will see the .zst and skip).
    let _ = file.sync_all();
    // BUG-89 TOCTOU: a resume-append during the window changed the source —
    // its new events would ride an unlinked inode and vanish. Drop this
    // attempt instead; the next pass retries on the grown file.
    match std::fs::metadata(path) {
        Ok(meta1) if (meta1.len(), meta1.modified().ok()) == fingerprint => {}
        _ => {
            let _ = std::fs::remove_file(&dest);
            return false;
        }
    }
    std::fs::remove_file(path).is_ok()
}

/// `foo.jsonl` → `foo.jsonl.zst` (appends after stripping the existing
/// `.jsonl` extension — `abc.jsonl` lands as `abc.jsonl.zst`).
fn path_with_extension(path: &Path, new_ext: &str) -> PathBuf {
    let stem = path.with_extension(""); // abc.jsonl → abc
    let mut s = stem.as_os_str().to_os_string();
    s.push(".");
    s.push(new_ext);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_file_compressed_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let log = sessions.join("abc.jsonl");
        std::fs::write(&log, "line1\nline2\n".repeat(100)).unwrap();

        // Backdate the mtime so the file looks cold.
        let old = SystemTime::now() - Duration::from_secs((SESSION_COLD_DAYS + 1) * 24 * 3600);
        let f = std::fs::File::options()
            .write(true)
            .open(&log)
            .unwrap();
        f.set_modified(old).unwrap();
        drop(f);

        let count = compress_cold_sessions(dir.path());
        assert_eq!(count, 1);
        assert!(!log.exists(), "original removed");
        let zst = sessions.join("abc.jsonl.zst");
        assert!(zst.exists(), "compressed file landed");

        // Decompression round-trips.
        let text = crate::jsonl::read_text_maybe_compressed_pub(&zst).unwrap();
        assert!(text.contains("line1"));
    }

    #[test]
    fn hot_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let log = sessions.join("hot.jsonl");
        std::fs::write(&log, "x".repeat(2000)).unwrap();

        let count = compress_cold_sessions(dir.path());
        assert_eq!(count, 0);
        assert!(log.exists());
    }
}
