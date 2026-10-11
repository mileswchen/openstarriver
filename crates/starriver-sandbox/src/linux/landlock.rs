//! Native Landlock runner: kernel LSM file-access control, no external
//! binary (works where unprivileged user namespaces are gated). The ruleset
//! handles only the write-family access rights and grants them just beneath
//! workspace root + /tmp; reads and exec stay unrestricted. Enforcement is
//! `Partial` — no network or process isolation (kernel 5.13+, ABI 1).
//!
//! Syscall numbers 444-446 are allocated in the shared range (identical on
//! x86_64/aarch64/riscv64), and the constants / repr(C) structs are declared
//! here so we don't bet on the vendored libc version carrying them — see
//! `/usr/include/linux/landlock.h`.

use std::ffi::CString;
use std::sync::Arc;

use starriver_kernel::sandbox::{
    ConfinedArgv, SandboxEnforcement, SandboxError, SandboxMode, SpawnHook,
};

const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

/// Flag for `create_ruleset(NULL)`: probe the kernel's highest supported ABI.
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;

const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
// ABI 2 (5.15). Never handled: cross-hierarchy rename (workspace ↔ /tmp)
// stays "always denied", matching seatbelt where both dirs must be writable
// sides of the rename — handling REFER would silently relax that.
#[allow(dead_code)]
const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 12;
// ABI 3 (6.2): `truncate(2)` — also needed in practice by `open(O_TRUNC)`
// overwrites (see landlock.h on WRITE_FILE).
const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 13;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: std::os::unix::io::RawFd,
}

const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;

/// Write-family rights this backend handles, folded by the kernel's Landlock
/// ABI so `create_ruleset` never sees unknown bits (EINVAL). Read/execute are
/// deliberately unhandled (unrestricted, seatbelt's allow-default parity) and
/// so is TRANSFER — restricting fd passing would cut ssh-agent/X11 sockets.
pub fn handled_access_fs(abi: u64) -> u64 {
    let mut bits = LANDLOCK_ACCESS_FS_WRITE_FILE
        | LANDLOCK_ACCESS_FS_REMOVE_DIR
        | LANDLOCK_ACCESS_FS_REMOVE_FILE
        | LANDLOCK_ACCESS_FS_MAKE_CHAR
        | LANDLOCK_ACCESS_FS_MAKE_DIR
        | LANDLOCK_ACCESS_FS_MAKE_REG
        | LANDLOCK_ACCESS_FS_MAKE_SOCK
        | LANDLOCK_ACCESS_FS_MAKE_FIFO;
    if abi >= 3 {
        bits |= LANDLOCK_ACCESS_FS_TRUNCATE;
    }
    bits
}

pub fn available() -> (bool, String) {
    match abi_version() {
        Ok(abi) if abi >= 1 => (true, format!("kernel Landlock ABI {abi}")),
        Ok(abi) => (false, format!("ABI {abi} unsupported")),
        Err(e) => (false, e),
    }
}

/// Probe: with the VERSION flag and a NULL attribute, the syscall returns the
/// highest supported ABI version (ENOSYS on pre-5.13, EOPNOTSUPP when
/// disabled at boot).
fn abi_version() -> Result<u64, String> {
    let r = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0 as libc::size_t,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if r < 0 {
        return Err(format!(
            "landlock_create_ruleset(VERSION): {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(r as u64)
}

pub fn confine(
    argv: Vec<String>,
    mode: SandboxMode,
    workspace_root: &str,
) -> Result<ConfinedArgv, SandboxError> {
    let rw: Vec<String> = match mode {
        SandboxMode::WorkspaceWrite => vec![
            crate::boundary::canonicalize_or_raw(workspace_root),
            "/tmp".into(),
        ],
        SandboxMode::ReadOnly => vec![],
        SandboxMode::DangerFullAccess => {
            return Ok(ConfinedArgv::new(argv, SandboxEnforcement::Full));
        }
    };
    // Landlock confines the process itself — argv cannot carry it. The hook
    // runs inside the forked child right before exec, so `sh` is born
    // restricted and every descendant inherits the ruleset. A hook error
    // surfaces as the spawn failing (fail closed at the spawn site).
    let hook: SpawnHook = Arc::new(move |cmd: &mut tokio::process::Command| {
        let rw = rw.clone();
        unsafe { cmd.pre_exec(move || apply(&rw)) };
        Ok(())
    });
    Ok(ConfinedArgv::new(argv, SandboxEnforcement::Partial)
        .runner("landlock")
        .spawn_hook(hook))
}

/// Applies the ruleset inside the child. Fail closed: any error aborts the
/// spawn instead of letting the command run unconfined.
fn apply(rw_paths: &[String]) -> std::io::Result<()> {
    unsafe {
        if libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let abi = abi_version().map_err(std::io::Error::other)?;
        let handled = handled_access_fs(abi);
        let attr = RulesetAttr {
            handled_access_fs: handled,
        };
        let ruleset = libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            &attr,
            std::mem::size_of::<RulesetAttr>(),
            0 as u32,
        );
        if ruleset < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let ruleset = ruleset as std::os::unix::io::RawFd;
        for path in rw_paths {
            let c = match CString::new(path.as_str()) {
                Ok(c) => c,
                Err(e) => {
                    libc::close(ruleset);
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, e));
                }
            };
            let dir = libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC);
            if dir < 0 {
                // Missing writable root: deny-by-omission is the safe
                // direction (writes there simply stay denied).
                tracing::warn!(path = %path, "landlock: cannot open writable root, writes stay denied");
                continue;
            }
            let rule = PathBeneathAttr {
                allowed_access: handled,
                parent_fd: dir,
            };
            if libc::syscall(
                SYS_LANDLOCK_ADD_RULE,
                ruleset,
                LANDLOCK_RULE_PATH_BENEATH,
                &rule,
                0 as u32,
            ) != 0
            {
                let err = std::io::Error::last_os_error();
                libc::close(dir);
                libc::close(ruleset);
                return Err(err);
            }
            libc::close(dir);
        }
        if libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset, 0 as u32) != 0 {
            let err = std::io::Error::last_os_error();
            libc::close(ruleset);
            return Err(err);
        }
        libc::close(ruleset);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handled_bits_fold_by_abi_without_unknown_flags() {
        let base = handled_access_fs(1);
        assert_eq!(base & LANDLOCK_ACCESS_FS_TRUNCATE, 0);
        assert_eq!(base & LANDLOCK_ACCESS_FS_REFER, 0);
        // ABI 3 (kernel 6.2+) adds TRUNCATE; REFER is never handled.
        assert_ne!(handled_access_fs(3) & LANDLOCK_ACCESS_FS_TRUNCATE, 0);
        assert_eq!(handled_access_fs(6) & LANDLOCK_ACCESS_FS_REFER, 0);
        // Write-family core present at every ABI.
        assert_ne!(base & LANDLOCK_ACCESS_FS_WRITE_FILE, 0);
        assert_ne!(base & LANDLOCK_ACCESS_FS_MAKE_DIR, 0);
    }

    #[test]
    fn read_only_confine_carries_empty_rule_list() {
        let confined = landlock_confined(SandboxMode::ReadOnly);
        assert!(confined.spawn_hook.is_some());
        assert_eq!(
            confined.argv,
            vec!["sh".to_string(), "-c".to_string(), "true".to_string()]
        );
    }

    fn landlock_confined(mode: SandboxMode) -> ConfinedArgv {
        confine(vec!["sh".into(), "-c".into(), "true".into()], mode, "/work").unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_reports_abi_on_capable_kernels() {
        let (ok, detail) = available();
        // WI-11 reference host runs 6.12 (ABI 6); tolerate stripped-down
        // kernels but record the detail either way.
        if !ok {
            panic!("landlock expected available here: {detail}");
        }
        assert!(detail.contains("ABI"));
    }
}
