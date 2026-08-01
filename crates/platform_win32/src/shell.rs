use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use anyhow::{bail, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const SE_ERR_NOASSOC: isize = 31;

// ShellExecuteW runs on a dedicated STA thread so COM shell extensions see a clean
// apartment. The call still blocks the caller until association resolution finishes;
// daemon event-loop sites must wrap this in spawn_blocking.
pub fn open(target: &OsStr) -> Result<()> {
    std::thread::scope(|scope| {
        match scope.spawn(|| open_on_com_thread(target)).join() {
            Ok(result) => result,
            Err(_) => bail!("ShellExecuteW worker thread panicked"),
        }
    })
}

fn open_on_com_thread(target: &OsStr) -> Result<()> {
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let result = open_with_shell(target);
    if hr.is_ok() {
        unsafe { CoUninitialize() };
    }
    result
}

fn open_with_shell(target: &OsStr) -> Result<()> {
    let target_wide = target_to_wide(target)?;
    let status = shell_execute(PCWSTR::null(), &target_wide);
    if shell_execute_succeeded(status) {
        return Ok(());
    }
    if should_retry_with_openas(status) {
        let status = shell_execute(w!("openas"), &target_wide);
        if shell_execute_succeeded(status) {
            return Ok(());
        }
        bail!(
            "ShellExecuteW failed to open {} with status {}",
            target.to_string_lossy(),
            status
        );
    }
    bail!(
        "ShellExecuteW failed to open {} with status {}",
        target.to_string_lossy(),
        status
    );
}

fn shell_execute(verb: PCWSTR, target_wide: &[u16]) -> isize {
    let result = unsafe {
        ShellExecuteW(
            None,
            verb,
            PCWSTR(target_wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    result.0 as isize
}

fn shell_execute_succeeded(status: isize) -> bool {
    status > 32
}

fn should_retry_with_openas(status: isize) -> bool {
    status == SE_ERR_NOASSOC
}

fn target_to_wide(target: &OsStr) -> Result<Vec<u16>> {
    let target_wide: Vec<u16> = target.encode_wide().collect();
    if target_wide.contains(&0) {
        bail!(
            "ShellExecuteW target contains an interior null: {}",
            target.to_string_lossy()
        );
    }
    Ok(target_wide.into_iter().chain(std::iter::once(0)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_to_wide_appends_null_terminator() {
        assert_eq!(
            target_to_wide(OsStr::new("C:\\Users\\José\\config.toml")).unwrap(),
            "C:\\Users\\José\\config.toml"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn target_to_wide_rejects_interior_null() {
        assert!(target_to_wide(OsStr::new("before\0after")).is_err());
    }

    #[test]
    fn shell_execute_success_is_above_32() {
        assert!(!shell_execute_succeeded(0));
        assert!(!shell_execute_succeeded(32));
        assert!(!shell_execute_succeeded(SE_ERR_NOASSOC));
        assert!(shell_execute_succeeded(33));
        assert!(shell_execute_succeeded(42));
    }

    #[test]
    fn openas_retry_only_on_no_association() {
        assert!(should_retry_with_openas(SE_ERR_NOASSOC));
        assert!(!should_retry_with_openas(0));
        assert!(!should_retry_with_openas(2));
        assert!(!should_retry_with_openas(33));
    }
}
