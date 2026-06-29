//! Integrity-level checks for UIPI-aware window management.
//!
//! Windows UIPI blocks a process from repositioning a window owned by a process
//! at a *higher mandatory integrity level*: a Medium-integrity (non-elevated)
//! daemon cannot move a High-integrity (elevated/admin) or System window —
//! `SetWindowPos` is silently refused. We detect that up front so the daemon
//! skips such a window instead of reserving a layout column it can never fill.
//!
//! Detection compares the target's integrity level against our own, which
//! subsumes UAC elevation (High vs Medium) and also catches non-elevated
//! higher-integrity owners (e.g. UIAccess, System) that a bare "is the token
//! elevated" check would miss.

use std::sync::OnceLock;
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, HANDLE, HWND};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

/// `SECURITY_MANDATORY_MEDIUM_RID` — the normal non-elevated integrity level.
/// Assumed for our own process if we can't read it, erring toward "we're low"
/// so higher-integrity windows are still blocked.
const INTEGRITY_MEDIUM: u32 = 0x2000;

/// Why a window can (or can't) be managed by this daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManageBlock {
    /// Manageable — same or lower integrity than us.
    No,
    /// Target runs at a higher integrity level (elevated/admin, or System).
    /// Running LeopardWM elevated lets it manage High-integrity windows.
    HigherIntegrity,
    /// Target's process can't be opened, or its token can't be read (protected /
    /// PPL / anti-cheat, or ACL-denied). Elevating LeopardWM may not help.
    Protected,
}

impl ManageBlock {
    /// Whether the window should be skipped (anything other than `No`).
    pub fn is_blocked(self) -> bool {
        !matches!(self, ManageBlock::No)
    }
}

/// Our own process integrity level (RID), cached. A process's integrity is
/// fixed for its lifetime.
fn daemon_integrity() -> u32 {
    static IL: OnceLock<u32> = OnceLock::new();
    *IL.get_or_init(|| unsafe { process_integrity(GetCurrentProcess()).unwrap_or(INTEGRITY_MEDIUM) })
}

/// Read a process handle's mandatory integrity level (the RID of the integrity
/// SID, e.g. 0x2000 Medium, 0x3000 High, 0x4000 System). `None` if the token or
/// label can't be read.
unsafe fn process_integrity(process: HANDLE) -> Option<u32> {
    let mut token = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
    let il = token_integrity(token);
    let _ = CloseHandle(token);
    il
}

unsafe fn token_integrity(token: HANDLE) -> Option<u32> {
    // Size probe, then read the TOKEN_MANDATORY_LABEL.
    let mut len = 0u32;
    let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut len);
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    GetTokenInformation(
        token,
        TokenIntegrityLevel,
        Some(buf.as_mut_ptr() as *mut _),
        len,
        &mut len,
    )
    .ok()?;
    // `buf` is a Vec<u8> (align 1) but TOKEN_MANDATORY_LABEL holds a pointer
    // (align 8); a direct reference would be a misaligned read (UB). Copy the
    // header out unaligned — its `Sid` pointer still points into `buf`, which
    // outlives the SID reads below.
    let label = std::ptr::read_unaligned(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
    let sid = label.Label.Sid;
    if sid.is_invalid() {
        return None;
    }
    let count = *GetSidSubAuthorityCount(sid);
    if count == 0 {
        return None;
    }
    // The integrity level is the SID's last sub-authority.
    Some(*GetSidSubAuthority(sid, (count - 1) as u32))
}

/// Whether (and why) Windows UIPI blocks this daemon from managing the window
/// owned by `pid`. `No` when the target is at our integrity level or lower.
pub fn manage_block(pid: u32) -> ManageBlock {
    let ours = daemon_integrity();
    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            // Access-denied on a limited-query open means a higher integrity or
            // a protected process we can't manage.
            Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => return ManageBlock::Protected,
            // Other failures (process exiting, stale pid) aren't a privilege
            // signal — don't skip a window over a transient race.
            Err(_) => return ManageBlock::No,
        };
        let target = process_integrity(handle);
        let _ = CloseHandle(handle);
        match target {
            Some(il) if il > ours => ManageBlock::HigherIntegrity,
            Some(_) => ManageBlock::No,
            // Opened but token/label unreadable → treat as protected.
            None => ManageBlock::Protected,
        }
    }
}

/// HWND-keyed convenience over [`manage_block`] for callers that hold a window
/// handle but no pid (e.g. restoring persisted layout, which stores only
/// HWNDs). `GetWindowThreadProcessId` is integrity-independent and only fails
/// for an invalid/dying HWND — and an actually higher-integrity window always
/// resolves — so an unresolvable pid is reported as `No` (manageable): never
/// drop a window we merely failed to identify over a transient race.
pub fn window_manage_block(hwnd: u64) -> ManageBlock {
    match window_process_id(hwnd) {
        Some(pid) => manage_block(pid),
        None => ManageBlock::No,
    }
}

fn window_process_id(hwnd: u64) -> Option<u32> {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(HWND(hwnd as *mut core::ffi::c_void), Some(&mut pid));
    }
    (pid != 0).then_some(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_manageable_by_itself() {
        // Our own process is at our own integrity level, so it's never blocked,
        // regardless of whether we're elevated.
        let our_pid = std::process::id();
        assert!(!manage_block(our_pid).is_blocked());
    }

    #[test]
    fn nonexistent_pid_is_not_blocked() {
        // A pid that fails to open for a reason other than access-denied must
        // not be reported as blocked, or a window whose process is racing
        // shutdown would be wrongly skipped. PID 0xFFFF_FFF0 is reserved/unused.
        assert_eq!(manage_block(0xFFFF_FFF0), ManageBlock::No);
    }
}
