//! Core types for the Win32 platform layer.

use leopardwm_core_layout::{Rect, WindowId};
use thiserror::Error;

/// Errors that can occur during Win32 operations.
#[derive(Debug, Error)]
pub enum Win32Error {
    #[error("Failed to enumerate windows: {0}")]
    EnumerationFailed(String),

    #[error("Failed to enumerate monitors: {0}")]
    MonitorEnumerationFailed(String),

    #[error("Failed to set window position: {0}")]
    SetPositionFailed(String),

    #[error("Failed to install event hook: {0}")]
    HookInstallFailed(String),

    #[error("Failed to register hotkey: {0}")]
    HotkeyRegistrationFailed(String),

    #[error("Window not found: {0}")]
    WindowNotFound(WindowId),
}

/// Information about a managed window.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    /// The window handle (HWND) as u64.
    pub hwnd: WindowId,
    /// Window title.
    pub title: String,
    /// Window class name.
    pub class_name: String,
    /// Process ID.
    pub process_id: u32,
    /// Current window rectangle.
    pub rect: Rect,
    /// Whether the window is visible.
    pub visible: bool,
}

/// Unique identifier for a monitor (derived from HMONITOR handle).
pub type MonitorId = isize;

/// Information about a display monitor.
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// Unique monitor identifier.
    pub id: MonitorId,
    /// Full monitor rectangle (entire display area).
    pub rect: Rect,
    /// Work area (excludes taskbar and other docked windows).
    pub work_area: Rect,
    /// Whether this is the primary monitor.
    pub is_primary: bool,
    /// Device name (e.g., `\\.\DISPLAY1`).
    pub device_name: String,
    /// DPI scale factor relative to 96 DPI (e.g., 1.0 for 100%, 2.0 for 200%).
    pub scale_factor: f64,
}

impl MonitorInfo {
    /// Check if a point is within this monitor's bounds.
    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        x >= self.rect.x
            && x < self.rect.x + self.rect.width
            && y >= self.rect.y
            && y < self.rect.y + self.rect.height
    }

    /// Check if a rectangle's center is within this monitor's bounds.
    pub fn contains_rect_center(&self, rect: &Rect) -> bool {
        let center_x = rect.x + rect.width / 2;
        let center_y = rect.y + rect.height / 2;
        self.contains_point(center_x, center_y)
    }
}

/// Configuration for the Win32 platform layer.
#[derive(Debug, Clone, Default)]
pub struct PlatformConfig {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_config_default() {
        let _config = PlatformConfig::default();
    }

    #[test]
    fn test_monitor_contains_point() {
        let monitor = MonitorInfo {
            id: 1,
            rect: Rect::new(0, 0, 1920, 1080),
            work_area: Rect::new(0, 0, 1920, 1040),
            is_primary: true,
            device_name: "DISPLAY1".to_string(),
            scale_factor: 1.0,
        };

        // Point inside monitor
        assert!(monitor.contains_point(960, 540));
        // Point at origin
        assert!(monitor.contains_point(0, 0));
        // Point just inside right edge
        assert!(monitor.contains_point(1919, 540));
        // Point outside (right edge)
        assert!(!monitor.contains_point(1920, 540));
        // Point outside (negative)
        assert!(!monitor.contains_point(-1, 0));
    }

    #[test]
    fn test_monitor_contains_rect_center() {
        let monitor = MonitorInfo {
            id: 1,
            rect: Rect::new(0, 0, 1920, 1080),
            work_area: Rect::new(0, 0, 1920, 1040),
            is_primary: true,
            device_name: "DISPLAY1".to_string(),
            scale_factor: 1.0,
        };

        // Window centered in monitor
        let window = Rect::new(100, 100, 800, 600);
        assert!(monitor.contains_rect_center(&window));

        // Window mostly outside but center inside
        let window2 = Rect::new(-300, 100, 800, 600);
        assert!(monitor.contains_rect_center(&window2)); // Center at 100, 400

        // Window with center outside
        let window3 = Rect::new(1800, 100, 800, 600);
        assert!(!monitor.contains_rect_center(&window3)); // Center at 2200, 400
    }

    #[test]
    fn test_win32_error_display() {
        // Verify error types have proper Display implementations
        let set_pos_err = Win32Error::SetPositionFailed("test error".to_string());
        let display = format!("{}", set_pos_err);
        assert!(display.contains("test error"));
        assert!(display.contains("position"));

        let window_not_found = Win32Error::WindowNotFound(12345);
        let display = format!("{}", window_not_found);
        assert!(display.contains("12345"));
    }
}
