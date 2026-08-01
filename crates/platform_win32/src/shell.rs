use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use anyhow::{bail, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

// ShellExecuteW inherits the caller's apartment state for COM-based shell extensions and synchronously blocks on association handlers.
pub fn open(target: &OsStr) -> Result<()> {
    let target_wide = target_to_wide(target)?;
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(target_wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let status = result.0 as isize;
    if status <= 32 {
        bail!(
            "ShellExecuteW failed to open {} with status {}",
            target.to_string_lossy(),
            status
        );
    }
    Ok(())
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
}
