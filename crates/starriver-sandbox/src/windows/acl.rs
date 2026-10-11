//! Idempotent grant-ACE application for WorkspaceWrite.
//!
//! Before each confined spawn, every writable root (workspace root, %TEMP%)
//! gets an allow ACE for the per-workspace synthetic SID granting
//! read|write|execute with object+container inheritance. The check runs
//! first, so the steady state costs one DACL read; nothing else in the DACL
//! is touched, and nothing is persisted — the SID is re-derived from the
//! root path (see `pure::workspace_sid`), which is what keeps this stateless
//! where needed `cap_sid.json` (and hit ACL-corruption incidents).
//!
//! Caveat, accepted for v1: subtrees with protected DACLs (PROTECTED_DACL
//! flag set by their creator) do not inherit the grant and stay write-denied.

use windows_sys::Win32::Foundation::{LocalFree, HLOCAL};
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    GetAce, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE, PSID,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_WRITE_DATA,
};
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

use super::token::{sid_from_string, sids_equal};

/// Read|write|execute — mirrors what the other backends' allow-lists imply
/// for a workspace directory.
const GRANT_MASK: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE;

/// Grants `sid_str` read|write|execute (with inheritance) on each path,
/// skipping paths whose DACL already carries a sufficient ACE for it. Only
/// ever ADDS one ACE; existing entries are left untouched.
pub fn ensure_write_aces(paths: &[&str], sid_str: &str) -> Result<(), String> {
    let grant_sid = sid_from_string(sid_str)?;
    for path in paths {
        if !ace_covers(path, grant_sid.0)? {
            add_grant_ace(path, grant_sid.0)?;
        }
    }
    Ok(())
}

/// True when `path`'s DACL already has an allow ACE for `sid` covering the
/// write-family bits (the rights `ensure_write_aces` exists to grant).
fn ace_covers(path: &str, sid: PSID) -> Result<bool, String> {
    let wide = super::wide(path);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: out-params; `sd` must be LocalFree'd on every return path.
    let err = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if err != 0 {
        return Err(format!(
            "starriver-sbx: GetNamedSecurityInfoW({path}) failed: {err}"
        ));
    }
    let covered = if dacl.is_null() {
        false
    } else {
        // SAFETY: `dacl` is valid per the call above; ACE layout is the
        // fixed winnt.h shape: 1-byte type, 1-byte flags, 2-byte size,
        // 4-byte mask, then the SID.
        unsafe {
            let count = (*dacl).AceCount;
            let mut found = false;
            for i in 0..count {
                let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
                if GetAce(dacl, i.into(), &mut ace) == 0 || ace.is_null() {
                    continue;
                }
                let bytes = ace as *const u8;
                if *bytes as u32 != ACCESS_ALLOWED_ACE_TYPE {
                    continue;
                }
                let mask = *(bytes.add(4) as *const u32);
                let ace_sid = bytes.add(8) as PSID;
                // The check must cover the whole write family ensure_write_aces
                // grants (GRANT_MASK) — a partial ACE (FILE_WRITE_DATA only)
                // was treated as covered and never upgraded, leaving appends
                // and renames denied inside the workspace.
                if (mask & GRANT_MASK) == GRANT_MASK && sids_equal(ace_sid, sid) {
                    found = true;
                    break;
                }
            }
            found
        }
    };
    // SAFETY: allocated by GetNamedSecurityInfoW, freed with LocalFree.
    unsafe { LocalFree(sd as HLOCAL) };
    Ok(covered)
}

/// Adds one allow ACE for `sid` on `path` (merging into the current DACL).
fn add_grant_ace(path: &str, sid: PSID) -> Result<(), String> {
    let wide = super::wide(path);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: same contract as ace_covers.
    let err = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if err != 0 {
        return Err(format!(
            "starriver-sbx: GetNamedSecurityInfoW({path}) failed: {err}"
        ));
    }

    // TRUSTEE_IS_SID carries the SID pointer in ptstrName (winnt.h pun).
    let trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0, // NO_MULTIPLE_TRUSTEE
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: sid as *mut u16,
    };
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: GRANT_MASK,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
        Trustee: trustee,
    };
    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `dacl`/`entry` valid; `new_dacl` is allocated on success and
    // must be LocalFree'd.
    let err = unsafe { SetEntriesInAclW(1, &entry, dacl, &mut new_dacl) };
    if err != 0 {
        // SAFETY: allocated above.
        unsafe { LocalFree(sd as HLOCAL) };
        return Err(format!(
            "starriver-sbx: SetEntriesInAclW({path}) failed: {err}"
        ));
    }
    // SAFETY: `new_dacl` from SetEntriesInAclW; ownership of the DACL
    // pointer passes to the security subsystem, `sd` still needs freeing.
    let err = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: allocated above.
    unsafe {
        LocalFree(new_dacl as HLOCAL);
        LocalFree(sd as HLOCAL);
    }
    if err != 0 {
        return Err(format!(
            "starriver-sbx: SetNamedSecurityInfoW({path}) failed: {err}"
        ));
    }
    Ok(())
}
