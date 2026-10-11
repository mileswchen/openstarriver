//! Helper-side execution: build the restricted token, apply grant ACEs
//! (WorkspaceWrite), spawn the child under the token via
//! `CreateProcessAsUserW` into a kill-on-close Job Object, wait, propagate
//! the exit code. Runs inside `starriver-sbx-helper.exe`.
//!
//! All failure paths print one `starriver-sbx: …` line to stderr and exit
//! 126 without ever running the child unconfined — the parent's denial
//! classifier maps that shape to `SANDBOX_CONFINE_FAILED`.

use starriver_kernel::sandbox::SandboxMode;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, GetExitCodeProcess, ResumeThread, TerminateProcess, WaitForSingleObject,
    CREATE_SUSPENDED, INFINITE, STARTF_USESTDHANDLES, STARTUPINFOW,
};

use super::{acl, job, pure, token};

/// Exit code the helper uses when the sandbox itself failed (the child never
/// ran) — bash's "command found but not executable" convention.
pub const SPAWN_FAILED: i32 = 126;

/// Probe rehearsal: exercise the full chain (token → confined spawn → job →
/// wait) with a harmless command. The parent probe chain calls this once per
/// process; healthy here means a real confined run works, not just that the
/// helper binary exists.
pub fn probe() -> i32 {
    let code = run(
        "read-only",
        "",
        &["cmd".into(), "/c".into(), "exit 0".into()],
    );
    if code == SPAWN_FAILED {
        // run() already printed the `starriver-sbx:` detail line.
        1
    } else {
        code
    }
}

/// One confined run. Returns the child's exit code, or `SPAWN_FAILED` when
/// the sandbox itself failed (the child never ran). `root` is only read for
/// workspace-write.
pub fn run(mode: &str, root: &str, child: &[String]) -> i32 {
    let Some(mode) = SandboxMode::parse(mode) else {
        eprintln!("starriver-sbx: unknown mode `{mode}`");
        return SPAWN_FAILED;
    };
    // WorkspaceWrite: grant the workspace SID write access on the writable
    // roots (workspace + %TEMP%) before spawning. Nothing is granted for
    // read-only; both paths fail closed.
    match mode {
        SandboxMode::WorkspaceWrite => {
            let sid = pure::workspace_sid(root);
            let temp = std::env::temp_dir();
            let temp = temp.to_string_lossy().into_owned();
            if let Err(e) = acl::ensure_write_aces(&[root, &temp], &sid) {
                eprintln!("{e}");
                return SPAWN_FAILED;
            }
            match spawn_confined(Some(&sid), child) {
                Ok(code) => code,
                Err(e) => {
                    eprintln!("{e}");
                    SPAWN_FAILED
                }
            }
        }
        _ => match spawn_confined(None, child) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("{e}");
                SPAWN_FAILED
            }
        },
    }
}

fn spawn_confined(grant_sid: Option<&str>, child: &[String]) -> Result<i32, String> {
    let tok = token::build_token(grant_sid)?;
    let job = job::create_kill_on_close_job()?;
    token::enable_spawn_privileges();

    let cmdline = pure::encode_cmdline(child);
    let mut cmdline_wide = super::wide(&cmdline);
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESTDHANDLES;
    // The helper's own stdio (the parent's pipes) must become the child's —
    // without STARTF_USESTDHANDLES + explicit handles, inheritance is
    // best-effort and output can vanish.
    si.hStdInput = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    si.hStdOutput = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    si.hStdError = unsafe { GetStdHandle(STD_ERROR_HANDLE) };

    let mut pi = unsafe { std::mem::zeroed() };
    // SAFETY: wide cmdline is NUL-terminated; stdio handles are owned by the
    // helper and stay valid; NULL env/cwd = inherit from the helper (which
    // itself inherited the session's cwd and environment from bash.rs).
    let ok = unsafe {
        CreateProcessAsUserW(
            tok.0,
            std::ptr::null(),
            cmdline_wide.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            1, // bInheritHandles
            CREATE_SUSPENDED,
            std::ptr::null(),
            std::ptr::null(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(token::last_err(&format!(
            "CreateProcessAsUserW(`{cmdline}`)"
        )));
    }
    if let Err(e) = job::assign(&job, pi.hProcess) {
        // SAFETY: spawned above; never let a failed job-assign run wild.
        unsafe { TerminateProcess(pi.hProcess, 1) };
        return Err(e);
    }
    // SAFETY: thread handle from CreateProcessAsUserW.
    unsafe { ResumeThread(pi.hThread) };

    // SAFETY: valid process handle.
    let wait = unsafe { WaitForSingleObject(pi.hProcess, INFINITE) };
    if wait != WAIT_OBJECT_0 {
        return Err(token::last_err("WaitForSingleObject"));
    }
    let mut code: u32 = 0;
    // SAFETY: valid process handle.
    let ok = unsafe { GetExitCodeProcess(pi.hProcess, &mut code) };
    if ok == 0 {
        return Err(token::last_err("GetExitCodeProcess"));
    }
    Ok(code as i32)
}
