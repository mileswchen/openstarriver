//! Job Object for the confined process tree.
//!
//! `KILL_ON_JOB_CLOSE` without any breakaway flag: when the helper dies
//! (bash.rs `kill_on_drop`, cancel, timeout) the last job handle closes and
//! the kernel reaps every remaining descendant — the same orphan-closing
//! guarantee the bwrap PID-namespace gives on Linux. ships
//! `BREAKAWAY_OK` by default and fastrender treats that as a bug; we follow
//! fastrender: a child must not be able to `CREATE_BREAKAWAY_FROM_JOB` its
//! way out of tree-kill.

use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

use super::token::{last_err, OwnedHandle};

pub(crate) fn create_kill_on_close_job() -> Result<OwnedHandle, String> {
    // SAFETY: name is NULL (unnamed job — nobody else can open/attach it).
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(last_err("CreateJobObjectW"));
    }
    let job = OwnedHandle(job);

    // windows-sys structs carry no Default; zeroed matches the C zero-init.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `limits` matches the JobObjectExtendedLimitInformation class.
    let ok = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(last_err("SetInformationJobObject"));
    }
    Ok(job)
}

/// Attaches a (suspended) child process to the job. Must happen before
/// `ResumeThread`, so no grandchild can slip out unassigned.
pub(crate) fn assign(
    job: &OwnedHandle,
    process: windows_sys::Win32::Foundation::HANDLE,
) -> Result<(), String> {
    // SAFETY: valid process/job handles from the spawn above.
    let ok = unsafe { AssignProcessToJobObject(job.0, process) };
    if ok == 0 {
        return Err(last_err("AssignProcessToJobObject"));
    }
    Ok(())
}
