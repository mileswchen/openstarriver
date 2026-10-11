//! Write-restricted token construction (the -unelevated model).
//!
//! `CreateRestrictedToken(WRITE_RESTRICTED)` makes every write-access check
//! run twice: once against the token's normal SIDs and once against the
//! restricting SID list — access needs BOTH to pass (Microsoft: "Restricted
//! Tokens"). Reads and network stay unrestricted, hence the honest
//! `Partial` enforcement of this backend.
//!
//! Two policy levels fall out directly:
//! - ReadOnly: the restricting list holds only our synthetic SID, which no
//!   DACL on earth grants → the second check fails everywhere. Zero ACL
//!   mutation. (NULL-DACL objects grant everyone everything and stay a known
//!   leak class — NULL DACLs are already broken practice.)
//! - WorkspaceWrite: the list adds the per-workspace synthetic SID, logon
//!   SID and Everyone, and grant ACEs for the workspace SID are applied to
//!   the workspace root + %TEMP% (see `acl.rs`).
//!
//! The token default DACL is set permissive (logon/Everyone/our SID, full
//! access) so objects the sandboxed process creates — pipes, sockets, temp
//! files — remain usable by it (hit ACCESS_DENIED inside PowerShell
//! pipelines without this).

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL, LUID};
use windows_sys::Win32::Security::{
    AddAccessAllowedAceEx, AdjustTokenPrivileges, CreateRestrictedToken, CreateWellKnownSid,
    DuplicateTokenEx, EqualSid, GetTokenInformation, InitializeAcl, LookupPrivilegeValueW,
    SecurityDelegation, SetTokenInformation, TokenDefaultDacl, TokenLogonSid, TokenPrimary,
    WinWorldSid, ACL, ACL_REVISION, DISABLE_MAX_PRIVILEGE, LUA_TOKEN, PSID, SECURITY_MAX_SID_SIZE,
    SE_PRIVILEGE_ENABLED, SID_AND_ATTRIBUTES, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL, TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_PRIVILEGES,
    TOKEN_QUERY, WRITE_RESTRICTED,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Win32 `GENERIC_ALL` — the generic-rights constants moved between
/// windows-sys releases; the numeric value is stable in winnt.h.
const GENERIC_ALL: u32 = 0x1000_0000;

/// Fixed-size SID buffer (`SECURITY_MAX_SID_SIZE` is a u32 const, which
/// Rust rejects in array-length position without the cast).
type SidBuf = [u8; SECURITY_MAX_SID_SIZE as usize];

/// Handle that closes itself. Every path here fails closed with an `Err`,
/// so the Drop is hygiene, not correctness.
pub(crate) struct OwnedHandle(pub HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A SID allocated by Win32 (`ConvertStringSidToSidW`) that frees itself
/// with `LocalFree` on drop.
pub(crate) struct OwnedSid(pub PSID);

impl Drop for OwnedSid {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0 as HLOCAL) };
        }
    }
}

/// Error helper: last OS error with our `starriver-sbx:` prefix, which the
/// parent's denial classifier treats as a backend startup failure
/// (`SANDBOX_CONFINE_FAILED`), never an ordinary command failure.
pub(crate) fn last_err(context: &str) -> String {
    format!(
        "starriver-sbx: {context}: {}",
        std::io::Error::last_os_error()
    )
}

/// `PSID` view into a fixed SID buffer (the buffer must outlive the use).
pub(crate) fn as_psid(buf: &SidBuf) -> PSID {
    buf.as_ptr() as PSID
}

/// Parses `S-1-…` into a Win32-allocated SID.
pub(crate) fn sid_from_string(s: &str) -> Result<OwnedSid, String> {
    let wide = crate::windows::wide(s);
    let mut psid: PSID = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated; Win32 allocates `psid` (LocalFree).
    let ok = unsafe {
        windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW(
            wide.as_ptr(),
            &mut psid,
        )
    };
    if ok == 0 || psid.is_null() {
        return Err(last_err("ConvertStringSidToSidW"));
    }
    Ok(OwnedSid(psid))
}

/// Byte-copies a raw SID into a fixed buffer (TokenLogonSid points into a
/// query buffer we want to own).
fn copy_sid(src: PSID) -> Result<SidBuf, String> {
    // SAFETY: `src` points at a valid SID; SubAuthorityCount precedes the
    // variable-length subauthority array.
    let len = unsafe {
        let count = (*(src as *const windows_sys::Win32::Security::SID)).SubAuthorityCount as usize;
        std::mem::size_of::<windows_sys::Win32::Security::SID>()
            + count.saturating_sub(1) * std::mem::size_of::<u32>()
    };
    if len > std::mem::size_of::<SidBuf>() {
        return Err("starriver-sbx: SID exceeds SECURITY_MAX_SID_SIZE".into());
    }
    let mut owned: SidBuf = [0; std::mem::size_of::<SidBuf>()];
    // SAFETY: len initialized bytes within the source SID.
    owned[..len].copy_from_slice(unsafe { std::slice::from_raw_parts(src as *const u8, len) });
    Ok(owned)
}

fn sid_to_buf(s: &str) -> Result<SidBuf, String> {
    let owned = sid_from_string(s)?;
    copy_sid(owned.0)
}

/// The well-known Everyone SID (S-1-1-0), built into a caller buffer.
fn everyone_sid() -> Result<SidBuf, String> {
    let mut buf: SidBuf = [0; std::mem::size_of::<SidBuf>()];
    let mut size = buf.len() as u32;
    // SAFETY: buffer is `size` bytes as the API contract requires.
    let ok = unsafe {
        CreateWellKnownSid(
            WinWorldSid,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut _,
            &mut size,
        )
    };
    if ok == 0 {
        return Err(last_err("CreateWellKnownSid"));
    }
    Ok(buf)
}

/// The logon session SID of the current process — part of the restricting
/// list so named-pipe/socket objects the sandboxed process creates (under
/// the permissive default DACL) also pass its own restricted check.
fn logon_sid(base: HANDLE) -> Result<SidBuf, String> {
    let mut buf: SidBuf = [0; std::mem::size_of::<SidBuf>()];
    let mut returned = 0u32;
    // SAFETY: TokenLogonSid writes a TOKEN_GROUPS into the buffer.
    let ok = unsafe {
        GetTokenInformation(
            base,
            TokenLogonSid,
            buf.as_mut_ptr() as *mut _,
            buf.len() as u32,
            &mut returned,
        )
    };
    if ok == 0 {
        return Err(last_err("GetTokenInformation(TokenLogonSid)"));
    }
    // SAFETY: buffer was just filled by the API above.
    let groups = unsafe { &*(&buf as *const SidBuf as *const TOKEN_GROUPS) };
    if groups.GroupCount == 0 {
        return Err("starriver-sbx: TokenLogonSid returned no groups".into());
    }
    copy_sid(groups.Groups[0].Sid)
}

/// Best-effort enable of the privileges `CreateProcessAsUserW` may need.
/// Elevated and service contexts hold SeAssignPrimaryTokenPrivilege and
/// SeIncreaseQuotaPrivilege *disabled* — the kernel's privilege check only
/// passes for enabled privileges, so they must be switched on before the
/// spawn. Unelevated UAC-filtered tokens don't hold them at all
/// (AdjustTokenPrivileges reports ERROR_NOT_ALL_ASSIGNED), which we swallow:
/// the spawn itself then fails with its own diagnostic and the probe reports
/// the backend unavailable.
pub(crate) fn enable_spawn_privileges() {
    for name in ["SeAssignPrimaryTokenPrivilege", "SeIncreaseQuotaPrivilege"] {
        let mut luid = LUID {
            LowPart: 0,
            HighPart: 0,
        };
        // SAFETY: `name` is a static str passed as NUL-terminated UTF-16.
        if unsafe {
            LookupPrivilegeValueW(
                std::ptr::null(),
                crate::windows::wide(name).as_ptr(),
                &mut luid,
            )
        } == 0
        {
            continue;
        }
        let mut base: HANDLE = std::ptr::null_mut();
        // SAFETY: out-param handle.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES, &mut base) } == 0
        {
            return;
        }
        let base = OwnedHandle(base);
        // SAFETY: all-zero is a valid TOKEN_PRIVILEGES for one entry; the
        // fields are fully assigned below.
        let mut tp: TOKEN_PRIVILEGES = unsafe { std::mem::zeroed() };
        tp.PrivilegeCount = 1;
        tp.Privileges[0].Luid = luid;
        tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;
        // SAFETY: `tp` describes exactly one enabled privilege; a FALSE
        // return or ERROR_NOT_ALL_ASSIGNED means the privilege is absent,
        // which is an expected unelevated condition, not a failure to report.
        unsafe {
            AdjustTokenPrivileges(
                base.0,
                0,
                &tp,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
    }
}

/// Builds the write-restricted primary token for one confined run.
///
/// `write_grant_sid` is `Some` for WorkspaceWrite (the per-workspace
/// synthetic SID that `acl.rs` grants on the writable roots) and `None` for
/// ReadOnly (restricting list = un-granted synthetic SID only). All SID
/// buffers outlive the `CreateRestrictedToken` call — they live to the end
/// of this function.
pub(crate) fn build_token(write_grant_sid: Option<&str>) -> Result<OwnedHandle, String> {
    let mut base: HANDLE = std::ptr::null_mut();
    // SAFETY: out-param handle, standard process-token query rights.
    let ok = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY,
            &mut base,
        )
    };
    if ok == 0 {
        return Err(last_err("OpenProcessToken"));
    }
    let base = OwnedHandle(base);

    let mut dup: HANDLE = std::ptr::null_mut();
    // SAFETY: duplicating our own process token into a primary token.
    let ok = unsafe {
        DuplicateTokenEx(
            base.0,
            // TOKEN_ADJUST_DEFAULT must be duplicated into the handle:
            // CreateRestrictedToken passes the input handle's access rights
            // through to the restricted token, and the default-DACL set below
            // needs it (otherwise ERROR_ACCESS_DENIED at SetTokenInformation).
            TOKEN_ASSIGN_PRIMARY | TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT,
            std::ptr::null(),
            SecurityDelegation,
            TokenPrimary,
            &mut dup,
        )
    };
    if ok == 0 {
        return Err(last_err("DuplicateTokenEx"));
    }
    let dup = OwnedHandle(dup);

    let everyone = everyone_sid()?;
    // Only WorkspaceWrite needs the logon SID; buffer lives past the
    // CreateRestrictedToken call below.
    let logon = if write_grant_sid.is_some() {
        Some(logon_sid(base.0)?)
    } else {
        None
    };

    let mut sid_bufs: Vec<OwnedSid> = Vec::new();
    let mut entries: Vec<SID_AND_ATTRIBUTES> = Vec::new();
    match write_grant_sid {
        Some(ws_sid) => {
            sid_bufs.push(sid_from_string(ws_sid)?);
            entries.push(SID_AND_ATTRIBUTES {
                Sid: sid_bufs.last().unwrap().0,
                Attributes: 0,
            });
            entries.push(SID_AND_ATTRIBUTES {
                Sid: as_psid(&everyone),
                Attributes: 0,
            });
            entries.push(SID_AND_ATTRIBUTES {
                Sid: as_psid(logon.as_ref().unwrap()),
                Attributes: 0,
            });
        }
        None => {
            // ReadOnly: a SID nothing grants. Attributes must be 0 —
            // "restricting SIDs are always enabled for access checks".
            sid_bufs.push(sid_from_string(&crate::windows::pure::readonly_sid())?);
            entries.push(SID_AND_ATTRIBUTES {
                Sid: sid_bufs.last().unwrap().0,
                Attributes: 0,
            });
        }
    }

    // LUA_TOKEN filters an elevated caller down to its standard-user variant
    // (CI runners are admins); tokens without a split variant reject the
    // flag, so retry without it.
    let mut token: HANDLE = std::ptr::null_mut();
    let mut ok = create_restricted(
        dup.0,
        DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED,
        &mut entries,
        &mut token,
    );
    if ok == 0 {
        ok = create_restricted(
            dup.0,
            DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED,
            &mut entries,
            &mut token,
        );
    }
    if ok == 0 {
        return Err(last_err("CreateRestrictedToken"));
    }
    let token = OwnedHandle(token);

    set_permissive_default_dacl(&token, write_grant_sid)?;
    Ok(token)
}

fn create_restricted(
    dup: HANDLE,
    flags: u32,
    entries: &mut [SID_AND_ATTRIBUTES],
    out: &mut HANDLE,
) -> i32 {
    // SAFETY: entries point at buffers owned by the caller for the duration
    // of this call.
    unsafe {
        CreateRestrictedToken(
            dup,
            flags,
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            entries.len() as u32,
            entries.as_ptr(),
            out,
        )
    }
}

/// Token default DACL = full access for Everyone + the granting SID + the
/// logon SID. Objects created by the sandboxed process inherit this DACL, so
/// it can reopen what it creates (pipes, sockets) despite the restricted
/// write check.
fn set_permissive_default_dacl(
    token: &OwnedHandle,
    write_grant_sid: Option<&str>,
) -> Result<(), String> {
    let mut aces: Vec<SidBuf> = Vec::new();
    aces.push(everyone_sid()?);
    if let Some(s) = write_grant_sid {
        aces.push(sid_to_buf(s)?);
    }
    aces.push(logon_sid(token.0)?);

    let mut acl_buf = [0u8; 256];
    let acl = acl_buf.as_mut_ptr() as *mut ACL;
    // SAFETY: ACL_REVISION header + three ACEs fit the 256-byte buffer.
    let ok = unsafe { InitializeAcl(acl, acl_buf.len() as u32, ACL_REVISION) };
    if ok == 0 {
        return Err(last_err("InitializeAcl"));
    }
    for sid in &aces {
        // SAFETY: `sid` holds a well-formed SID copy.
        let ok = unsafe { AddAccessAllowedAceEx(acl, ACL_REVISION, 0, GENERIC_ALL, as_psid(sid)) };
        if ok == 0 {
            return Err(last_err("AddAccessAllowedAceEx"));
        }
    }
    let default_dacl = TOKEN_DEFAULT_DACL { DefaultDacl: acl };
    // SAFETY: the ACL buffer outlives this call; the token copies the DACL.
    let ok = unsafe {
        SetTokenInformation(
            token.0,
            TokenDefaultDacl,
            &default_dacl as *const TOKEN_DEFAULT_DACL as *const _,
            std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
        )
    };
    if ok == 0 {
        return Err(last_err("SetTokenInformation(TokenDefaultDacl)"));
    }
    Ok(())
}

/// True when two raw SIDs are byte-equal — `acl.rs` uses this for idempotent
/// ACE detection.
pub(crate) fn sids_equal(a: PSID, b: PSID) -> bool {
    // SAFETY: both point at well-formed SIDs.
    unsafe { EqualSid(a, b) != 0 }
}
