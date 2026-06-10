//! System-level queries: DPI scaling, power state, accessibility settings.

/// Scale a pixel value by the given DPI scale factor.
///
/// Config values are in logical pixels (96 DPI). This function converts them
/// to physical pixels for a specific monitor's DPI.
pub fn scale_px(value: i32, scale_factor: f64) -> i32 {
    (value as f64 * scale_factor).round() as i32
}

/// Check if the system is running on battery power or Windows power saver is active.
/// Returns `true` when either condition is met, signalling that animations should be disabled.
pub fn is_on_battery_or_power_saver() -> bool {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    let mut status = SYSTEM_POWER_STATUS::default();
    unsafe {
        if GetSystemPowerStatus(&mut status).is_ok() {
            // ACLineStatus: 0 = offline (battery), 1 = online (AC), 255 = unknown
            let on_battery = status.ACLineStatus == 0;
            // SystemStatusFlag bit 0: Windows power saver is active
            let power_saver = (status.SystemStatusFlag & 1) != 0;
            on_battery || power_saver
        } else {
            false // Assume AC if the API fails
        }
    }
}

/// Check if Windows "Show animations" accessibility setting is enabled.
/// Returns `false` when the user has disabled client-area animations
/// (Settings > Accessibility > Visual effects > Animation effects).
pub fn are_animations_enabled() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::SystemParametersInfoW;
    use windows::Win32::UI::WindowsAndMessaging::SPI_GETCLIENTAREAANIMATION;
    use windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS;

    let mut enabled: i32 = 1;
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut enabled as *mut i32 as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    enabled != 0
}

/// Check if Windows High Contrast mode is enabled.
/// Returns `true` when the user has activated a high contrast theme
/// (Settings > Accessibility > Contrast themes).
pub fn is_high_contrast_enabled() -> bool {
    use windows::Win32::UI::Accessibility::{HIGHCONTRASTW, HIGHCONTRASTW_FLAGS, HCF_HIGHCONTRASTON};
    use windows::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS};

    // SPI_GETHIGHCONTRAST = 0x0042
    const SPI_GETHIGHCONTRAST: u32 = 0x0042;

    let mut hc = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    unsafe {
        let _ = SystemParametersInfoW(
            windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_ACTION(SPI_GETHIGHCONTRAST),
            hc.cbSize,
            Some(&mut hc as *mut HIGHCONTRASTW as *mut std::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    (hc.dwFlags & HCF_HIGHCONTRASTON) != HIGHCONTRASTW_FLAGS(0)
}

/// Get the system highlight color as a BGR COLORREF value.
/// Used in high contrast mode to override the border color with the
/// system-defined highlight color, matching native Windows behavior.
pub fn get_system_highlight_color_bgr() -> u32 {
    use windows::Win32::Graphics::Gdi::GetSysColor;

    // COLOR_HIGHLIGHT = 13
    unsafe { GetSysColor(windows::Win32::Graphics::Gdi::SYS_COLOR_INDEX(13)) }
}

/// Set the process DPI awareness to Per-Monitor Aware V2.
///
/// This must be called as early as possible in `main()`, before any
/// window or GDI operations. Returns `true` if the call succeeded.
pub fn set_dpi_awareness() -> bool {
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scale_px_identity_at_100_percent() {
        assert_eq!(scale_px(10, 1.0), 10);
        assert_eq!(scale_px(0, 1.0), 0);
        assert_eq!(scale_px(-5, 1.0), -5);
    }

    #[test]
    fn test_scale_px_200_percent() {
        assert_eq!(scale_px(10, 2.0), 20);
        assert_eq!(scale_px(3, 2.0), 6);
    }

    #[test]
    fn test_scale_px_150_percent_rounds() {
        assert_eq!(scale_px(3, 1.5), 5); // 4.5 rounds to 5
        assert_eq!(scale_px(10, 1.5), 15);
        assert_eq!(scale_px(1, 1.5), 2); // 1.5 rounds to 2
    }

    #[test]
    fn test_scale_px_125_percent() {
        assert_eq!(scale_px(10, 1.25), 13); // 12.5 rounds to 13
        assert_eq!(scale_px(8, 1.25), 10);
    }

    #[test]
    #[ignore = "Requires display hardware - run with: cargo test -- --ignored"]
    fn test_set_dpi_awareness_no_panic() {
        // On CI/test environments this may return false (already set), but must not panic
        let _result = set_dpi_awareness();
    }
}
