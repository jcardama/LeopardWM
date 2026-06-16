//! Security attributes for the IPC named pipe.
//!
//! A named pipe inherits the creating process's integrity level. When the
//! daemon runs elevated (high integrity), a non-elevated client (medium) is
//! blocked from connecting. We build a security descriptor that grants the
//! current user full access and labels the pipe at MEDIUM integrity, so a
//! non-elevated same-user client can connect even to an elevated daemon.
//! Building falls back to `None` on any failure, in which case the caller
//! creates the pipe with default attributes.

use std::ffi::c_void;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Owns a security descriptor + `SECURITY_ATTRIBUTES` for the IPC pipe. The
/// `SECURITY_ATTRIBUTES` is boxed so its address stays stable while the pipe
/// server reuses it across pipe-instance creations. Frees the descriptor on
/// drop.
pub struct PipeSecurityAttributes {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: Box<SECURITY_ATTRIBUTES>,
}

impl PipeSecurityAttributes {
    /// Build the pipe security attributes for the current user, or `None` if
    /// any step fails (caller should fall back to default pipe creation).
    pub fn new() -> Option<Self> {
        let sid = current_user_sid_string()?;
        // Grant the current user and SYSTEM full access; label the pipe at
        // medium integrity (ML/no-write-up/ME) so a non-elevated medium client
        // can connect even when the daemon created the pipe while elevated.
        let sddl = format!("D:(A;;FA;;;{sid})(A;;FA;;;SY)S:(ML;;NW;;;ME)");
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();

        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
            .ok()?;
        }

        let attributes = Box::new(SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        });
        Some(Self {
            descriptor,
            attributes,
        })
    }

    /// Raw pointer to the `SECURITY_ATTRIBUTES`, for
    /// `ServerOptions::create_with_security_attributes_raw`.
    pub fn as_ptr(&self) -> *mut c_void {
        std::ptr::from_ref::<SECURITY_ATTRIBUTES>(&self.attributes).cast_mut() as *mut c_void
    }
}

impl Drop for PipeSecurityAttributes {
    fn drop(&mut self) {
        if !self.descriptor.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.descriptor.0)));
            }
        }
    }
}

/// The current process user's SID as an SDDL string (e.g. `S-1-5-21-...`).
fn current_user_sid_string() -> Option<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;

        // First call sizes the buffer (fails with insufficient-buffer; expected).
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        if len == 0 {
            let _ = CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        let res = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut c_void),
            len,
            &mut len,
        );
        let _ = CloseHandle(token);
        res.ok()?;

        let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut psid = PWSTR::null();
        ConvertSidToStringSidW(token_user.User.Sid, &mut psid).ok()?;
        let s = psid.to_string().ok();
        if !psid.is_null() {
            let _ = LocalFree(Some(HLOCAL(psid.0 as *mut c_void)));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_sid_is_resolvable() {
        let sid = current_user_sid_string().expect("current user SID resolves");
        assert!(sid.starts_with("S-1-"), "expected a SID string, got {sid}");
    }

    #[test]
    fn pipe_security_attributes_build() {
        let sec = PipeSecurityAttributes::new().expect("pipe security attributes build");
        assert!(!sec.as_ptr().is_null(), "security attributes pointer is non-null");
    }
}
