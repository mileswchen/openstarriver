//! Denial signature normalization: after a confined command exits non-zero,
//! distinguish "the sandbox denied it" from "the command itself failed" and
//! surface one stable message shape to the model and UI, whatever the
//! backend (Seatbelt → EPERM, bubblewrap → EROFS, Landlock/writetoken →
//! EACCES).
//!
//! Classification is deliberately conservative: an errno signature only maps
//! to a sandbox denial for runners where that errno is the confinement
//! hallmark (a generic EACCES under bubblewrap is usually the app's own
//! permissions, not the sandbox).

/// A normalized sandbox denial (wire code `SANDBOX_DENIED`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialSignature {
    pub runner: String,
    /// errno-style tag: "EROFS" | "EPERM" | "EACCES".
    pub errno_tag: &'static str,
    pub op: &'static str,
    /// Best-effort target path parsed from the failing stderr line.
    pub path: Option<String>,
}

/// True when the sandbox backend itself failed to start (as opposed to the
/// command being denied inside a working sandbox) — the caller must surface
/// `SANDBOX_CONFINE_FAILED` and never fall back to unconfined execution.
/// A genuine bwrap startup failure leaves stdout empty; command-generated
/// noise on stderr (e.g. `echo bwrap:`) comes with non-empty stdout. The
/// Windows helper (`starriver-sbx-helper`) follows the same shape with its
/// `starriver-sbx: ` prefix and exit 126.
pub fn runner_startup_failed(runner: &str, stdout: &str, stderr: &str) -> bool {
    match runner {
        "bwrap" => {
            stdout.is_empty()
                && stderr
                    .lines()
                    .any(|l| l.trim_start().starts_with("bwrap: "))
        }
        "writetoken" => {
            stdout.is_empty()
                && stderr
                    .lines()
                    .any(|l| l.trim_start().starts_with("starriver-sbx: "))
        }
        _ => false,
    }
}

pub fn message(sig: &DenialSignature) -> String {
    format!(
        "sandbox denied {} on {} (backend={}, fs={})",
        sig.op,
        sig.path.as_deref().unwrap_or("unknown target"),
        sig.runner,
        sig.errno_tag
    )
}

/// Scan the confined command's stderr for a sandbox denial signature.
/// Lines are checked last-first: the decisive error is usually the last one
/// printed. Returns `None` when nothing matches — the failure is then an
/// ordinary `EXECUTION_FAILED`, not a boundary event. Unconfined runs
/// (`runner == "none"`) never classify: a plain read-only filesystem hit is
/// then the app's own business, not a boundary event.
pub fn classify(runner: &str, stderr: &str) -> Option<DenialSignature> {
    if runner == "none" {
        return None;
    }
    stderr
        .lines()
        .rev()
        .find_map(|line| line_signature(runner, line))
}

fn line_signature(runner: &str, line: &str) -> Option<DenialSignature> {
    let (tag, op) = if line.contains("Read-only file system") {
        // bubblewrap ro-bind mount (any runner could surface it — a ro fs
        // under confinement behaves identically to a ro device).
        ("EROFS", "write")
    } else if runner == "seatbelt" && line.contains("Operation not permitted") {
        // Seatbelt denials surface as EPERM.
        ("EPERM", "write")
    } else if runner == "landlock" && line.contains("Permission denied") {
        // Landlock denies writes with EACCES; reads are never restricted by
        // this backend, so EACCES under it is a boundary event.
        ("EACCES", "write")
    } else if runner == "writetoken"
        && (line.contains("Permission denied") || line.contains("Access is denied"))
    {
        // Write-restricted token denies surface as MSYS `Permission denied`
        // (git-bash) or native `Access is denied.` (cmd.exe). Reads are
        // never restricted by this backend, so either is a boundary event.
        ("EACCES", "write")
    } else {
        return None;
    };
    Some(DenialSignature {
        runner: runner.to_string(),
        errno_tag: tag,
        op,
        path: quoted_path(line),
    })
}

/// Best-effort target path from the failing line: a single-quoted token
/// (GNU coreutils style) or the first `/`- or `\`-containing token with
/// trailing punctuation stripped (BSD style: `touch: /tmp/x: Operation not
/// permitted`; Windows style: `touch: C:\tmp\x: Permission denied`).
fn quoted_path(line: &str) -> Option<String> {
    if let Some(quoted) = line.split('\'').nth(1) {
        return Some(quoted.to_string());
    }
    line.split_whitespace()
        .find(|t| t.contains('/') || t.contains('\\'))
        .map(|t| t.trim_matches(|c| c == ':' || c == ',').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bwrap_ro_bind_maps_to_sandbox_denied() {
        let stderr = "touch: setting times of '/etc/hostname': Read-only file system\n";
        let sig = classify("bwrap", stderr).unwrap();
        assert_eq!(sig.errno_tag, "EROFS");
        assert_eq!(sig.path.as_deref(), Some("/etc/hostname"));
        assert_eq!(
            message(&sig),
            "sandbox denied write on /etc/hostname (backend=bwrap, fs=EROFS)"
        );
    }

    #[test]
    fn seatbelt_eperm_maps_to_sandbox_denied() {
        let sig = classify("seatbelt", "touch: /tmp/x: Operation not permitted\n").unwrap();
        assert_eq!(sig.errno_tag, "EPERM");
        assert_eq!(sig.path.as_deref(), Some("/tmp/x"));
    }

    #[test]
    fn landlock_eacces_maps_to_sandbox_denied() {
        let sig = classify(
            "landlock",
            "sh: 1: cannot create /etc/hosts: Permission denied\n",
        )
        .unwrap();
        assert_eq!(sig.errno_tag, "EACCES");
    }

    #[test]
    fn eacces_under_other_runners_stays_execution_failed() {
        // Generic permission errors are usually the app's own business.
        assert_eq!(classify("bwrap", "cat: /root/x: Permission denied"), None);
        assert_eq!(
            classify("seatbelt", "cat: /root/x: Permission denied"),
            None
        );
        assert_eq!(classify("none", "cat: /root/x: Permission denied"), None);
    }

    #[test]
    fn last_line_wins_when_stderr_has_noise() {
        let stderr =
            "make: entering directory\nrm: cannot remove '/usr/x': Read-only file system\n";
        assert_eq!(
            classify("bwrap", stderr).unwrap().path.as_deref(),
            Some("/usr/x")
        );
    }

    #[test]
    fn plain_failures_do_not_classify() {
        assert_eq!(
            classify("bwrap", "ls: /nope: No such file or directory"),
            None
        );
        assert_eq!(
            classify("landlock", "git: 'foo' is not a git command"),
            None
        );
        assert_eq!(classify("seatbelt", ""), None);
        // Unconfined: even EROFS is the app's own business.
        assert_eq!(
            classify("none", "touch: '/media/cdrom/x': Read-only file system"),
            None
        );
    }

    #[test]
    fn bwrap_startup_failure_is_confine_not_denial() {
        let stderr = "bwrap: Setting up uid map: Permission denied\n";
        assert!(runner_startup_failed("bwrap", "", stderr));
        // Command-echoed noise with stdout traffic is not a startup failure.
        assert!(!runner_startup_failed("bwrap", "bwrap: hi\n", stderr));
        // stderr noise not led by the backend prefix either.
        assert!(!runner_startup_failed("bwrap", "", "error: bwrap vanished"));
        assert!(!runner_startup_failed("landlock", "", stderr));
    }
}
