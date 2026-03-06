//! LeopardWM IPC Protocol
//!
//! Shared types for daemon-CLI communication over Windows named pipes.

use serde::{Deserialize, Serialize};

/// Named pipe path for IPC communication.
pub const PIPE_NAME: &str = r"\\.\pipe\leopardwm";
/// Maximum length of sanitized user scope appended to the pipe path.
const MAX_PIPE_SCOPE_SEGMENT_LEN: usize = 64;

/// IPC protocol version for lightweight compatibility checks.
pub const IPC_PROTOCOL_VERSION: u32 = 1;
/// Minimum protocol version this crate supports.
pub const IPC_MIN_SUPPORTED_PROTOCOL_VERSION: u32 = 1;

/// Maximum IPC message size (64 KiB). Messages larger than this are rejected.
pub const MAX_IPC_MESSAGE_SIZE: usize = 64 * 1024;

fn sanitize_pipe_scope_segment(scope: &str) -> String {
    let mut sanitized = String::with_capacity(scope.len());
    for ch in scope.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            sanitized.push(ch.to_ascii_lowercase());
        } else {
            sanitized.push('_');
        }
        if sanitized.len() >= MAX_PIPE_SCOPE_SEGMENT_LEN {
            break;
        }
    }
    sanitized.trim_matches('_').to_string()
}

/// Build a user-scoped pipe name from an arbitrary user/domain scope string.
pub fn scoped_pipe_name_for_user(scope: &str) -> String {
    let segment = sanitize_pipe_scope_segment(scope);
    if segment.is_empty() {
        PIPE_NAME.to_string()
    } else {
        format!("{PIPE_NAME}_{segment}")
    }
}

/// Preferred pipe name for this process/user with legacy fallback available.
///
/// Resolution order:
/// 1. `LEOPARDWM_PIPE_SCOPE` environment override
/// 2. `USERDOMAIN\\USERNAME`
/// 3. legacy global `PIPE_NAME`
pub fn preferred_pipe_name() -> String {
    if let Ok(scope) = std::env::var("LEOPARDWM_PIPE_SCOPE") {
        let scoped = scoped_pipe_name_for_user(&scope);
        if scoped != PIPE_NAME {
            return scoped;
        }
    }

    let domain = std::env::var("USERDOMAIN").ok();
    let user = std::env::var("USERNAME").ok();
    match (domain, user) {
        (Some(domain), Some(user)) if !domain.trim().is_empty() && !user.trim().is_empty() => {
            scoped_pipe_name_for_user(&format!("{domain}\\{user}"))
        }
        _ => PIPE_NAME.to_string(),
    }
}

/// Candidate pipe names in preference order with legacy compatibility fallback.
pub fn pipe_name_candidates() -> Vec<String> {
    let preferred = preferred_pipe_name();
    if preferred == PIPE_NAME {
        vec![preferred]
    } else {
        vec![preferred, PIPE_NAME.to_string()]
    }
}

/// Return the protocol identifier for this crate version.
pub fn protocol_id() -> u32 {
    IPC_PROTOCOL_VERSION
}

/// Whether a remote protocol version is supported by this crate.
pub fn is_protocol_version_supported(version: u32) -> bool {
    (IPC_MIN_SUPPORTED_PROTOCOL_VERSION..=IPC_PROTOCOL_VERSION).contains(&version)
}

/// Rectangle for IPC serialization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl IpcRect {
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Detailed information about a window for IPC queries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    /// The window handle as a unique identifier.
    pub window_id: u64,
    /// The window's current title.
    pub title: String,
    /// The window's class name.
    pub class_name: String,
    /// The process ID that owns this window.
    pub process_id: u32,
    /// The executable name (e.g., "notepad.exe").
    pub executable: String,
    /// The window's current rectangle (position and size).
    pub rect: IpcRect,
    /// The column index if tiled, None if floating.
    pub column_index: Option<usize>,
    /// The window index within its column, None if floating.
    pub window_index: Option<usize>,
    /// The monitor ID this window is on.
    pub monitor_id: i64,
    /// Whether this window is floating (not tiled).
    pub is_floating: bool,
    /// Whether this window currently has focus.
    pub is_focused: bool,
}

/// Commands that can be sent from the CLI to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcCommand {
    /// Focus the column to the left.
    FocusLeft,
    /// Focus the column to the right.
    FocusRight,
    /// Focus the window above (in stacked columns).
    FocusUp,
    /// Focus the window below (in stacked columns).
    FocusDown,

    /// Move the focused column left.
    MoveColumnLeft,
    /// Move the focused column right.
    MoveColumnRight,

    /// Move the focused window to the column on the left.
    MoveWindowLeft,
    /// Move the focused window to the column on the right.
    MoveWindowRight,
    /// Expel the focused window to a new column on the left.
    ExpelToLeft,
    /// Expel the focused window to a new column on the right.
    ExpelToRight,
    /// Move the focused window up within the column.
    MoveWindowUp,
    /// Move the focused window down within the column.
    MoveWindowDown,

    /// Focus the monitor to the left.
    FocusMonitorLeft,
    /// Focus the monitor to the right.
    FocusMonitorRight,
    /// Move the focused window to the monitor on the left.
    MoveWindowToMonitorLeft,
    /// Move the focused window to the monitor on the right.
    MoveWindowToMonitorRight,

    /// Resize the focused column.
    Resize {
        /// Width delta in pixels (positive to grow, negative to shrink).
        delta: i32,
    },

    /// Scroll the viewport.
    Scroll {
        /// Scroll delta (positive = right, negative = left).
        delta: f64,
    },

    /// Query the current workspace state.
    QueryWorkspace,
    /// Query the focused window.
    QueryFocused,

    /// Re-enumerate windows and add new ones.
    Refresh,
    /// Apply the current layout to windows.
    Apply,
    /// Reload configuration from file.
    Reload,
    /// Stop the daemon.
    Stop,
    /// Emergency recovery command to revert managed windows to a safe state.
    PanicRevert,
    /// Toggle paused state for tiling operations.
    TogglePause,

    /// Query detailed information about all managed windows.
    QueryAllWindows,

    /// Close the focused window.
    CloseWindow,
    /// Toggle floating state for the focused window.
    ToggleFloating,
    /// Toggle fullscreen for the focused window.
    ToggleFullscreen,
    /// Set the focused column width as a fraction of the viewport.
    SetColumnWidth {
        /// Fraction of viewport width (e.g., 0.333, 0.5, 0.667).
        fraction: f64,
    },
    /// Equalize all column widths.
    EqualizeColumnWidths,
    /// Cycle focused column width up through presets.
    CycleWidthUp,
    /// Cycle focused column width down through presets.
    CycleWidthDown,
    /// Cycle focused window height up through presets.
    CycleHeightUp,
    /// Cycle focused window height down through presets.
    CycleHeightDown,
    /// Equalize height weights in the focused column.
    EqualizeColumnHeights,
    /// Query daemon status information.
    QueryStatus,
    /// Health check — returns uptime, window count, and error count.
    HealthCheck,
}

/// Responses from the daemon to the CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum IpcResponse {
    /// Command executed successfully.
    Ok,
    /// Command failed with an error.
    Error {
        /// Error message describing what went wrong.
        message: String,
    },
    /// Workspace state query response.
    WorkspaceState {
        /// Number of columns in the workspace.
        columns: usize,
        /// Total number of windows.
        windows: usize,
        /// Index of the currently focused column.
        focused_column: usize,
        /// Index of the focused window within its column.
        focused_window: usize,
        /// Current scroll offset.
        scroll_offset: f64,
        /// Total width of all columns.
        total_width: i32,
    },
    /// Focused window query response.
    FocusedWindow {
        /// Window ID of the focused window, if any.
        window_id: Option<u64>,
        /// Column index of the focused window.
        column_index: usize,
        /// Window index within the column.
        window_index: usize,
    },

    /// Response containing information about all windows.
    WindowList {
        /// List of all managed windows.
        windows: Vec<WindowInfo>,
    },

    /// Response containing detailed info about the focused window.
    FocusedWindowInfo {
        /// The focused window's info, if any.
        window: Option<WindowInfo>,
    },

    /// Daemon status information.
    StatusInfo {
        /// Daemon version.
        version: String,
        /// Number of monitors.
        monitors: usize,
        /// Total managed windows across all workspaces.
        total_windows: usize,
        /// Daemon uptime in seconds.
        uptime_seconds: u64,
    },
    /// Health check response.
    HealthInfo {
        /// Whether the daemon considers itself healthy.
        healthy: bool,
        /// Daemon uptime in seconds.
        uptime_seconds: u64,
        /// Total managed windows.
        total_windows: usize,
        /// Number of monitors detected.
        monitors: usize,
        /// Whether tiling is paused.
        paused: bool,
    },
    /// Forward-compatibility fallback for newer daemon responses unknown to this client.
    #[serde(other)]
    Unknown,
}

impl IpcResponse {
    /// Create an error response.
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_serialization() {
        let cmd = IpcCommand::FocusLeft;
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("focus_left"));

        let cmd2: IpcCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, cmd2);
    }

    #[test]
    fn test_resize_command_serialization() {
        let cmd = IpcCommand::Resize { delta: -50 };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("resize"));
        assert!(json.contains("-50"));

        let cmd2: IpcCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, cmd2);
    }

    #[test]
    fn test_panic_revert_wire_name_is_stable() {
        let cmd = IpcCommand::PanicRevert;
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#"{"type":"panic_revert"}"#);

        let parsed: IpcCommand = serde_json::from_str(r#"{"type":"panic_revert"}"#).unwrap();
        assert_eq!(parsed, IpcCommand::PanicRevert);
    }

    #[test]
    fn test_response_serialization() {
        let resp = IpcResponse::Ok;
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("ok"));

        let resp2: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, resp2);
    }

    #[test]
    fn test_workspace_state_serialization() {
        let resp = IpcResponse::WorkspaceState {
            columns: 3,
            windows: 5,
            focused_column: 1,
            focused_window: 0,
            scroll_offset: 100.5,
            total_width: 2400,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("workspace_state"));
        assert!(json.contains("\"columns\":3"));

        let resp2: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, resp2);
    }

    #[test]
    fn test_error_response() {
        let resp = IpcResponse::error("Something went wrong");
        if let IpcResponse::Error { message } = resp {
            assert_eq!(message, "Something went wrong");
        } else {
            panic!("Expected Error response");
        }
    }

    #[test]
    fn test_all_command_types_roundtrip() {
        // Verify all command variants serialize and deserialize correctly
        let commands = vec![
            IpcCommand::FocusLeft,
            IpcCommand::FocusRight,
            IpcCommand::FocusUp,
            IpcCommand::FocusDown,
            IpcCommand::MoveColumnLeft,
            IpcCommand::MoveColumnRight,
            IpcCommand::MoveWindowLeft,
            IpcCommand::MoveWindowRight,
            IpcCommand::ExpelToLeft,
            IpcCommand::ExpelToRight,
            IpcCommand::MoveWindowUp,
            IpcCommand::MoveWindowDown,
            IpcCommand::FocusMonitorLeft,
            IpcCommand::FocusMonitorRight,
            IpcCommand::MoveWindowToMonitorLeft,
            IpcCommand::MoveWindowToMonitorRight,
            IpcCommand::Resize { delta: 100 },
            IpcCommand::Resize { delta: -50 },
            IpcCommand::Scroll { delta: 150.5 },
            IpcCommand::Scroll { delta: -75.0 },
            IpcCommand::QueryWorkspace,
            IpcCommand::QueryFocused,
            IpcCommand::QueryAllWindows,
            IpcCommand::Refresh,
            IpcCommand::Apply,
            IpcCommand::Reload,
            IpcCommand::Stop,
            IpcCommand::PanicRevert,
            IpcCommand::TogglePause,
            IpcCommand::CloseWindow,
            IpcCommand::ToggleFloating,
            IpcCommand::ToggleFullscreen,
            IpcCommand::SetColumnWidth { fraction: 0.5 },
            IpcCommand::SetColumnWidth { fraction: 0.333 },
            IpcCommand::EqualizeColumnWidths,
            IpcCommand::CycleWidthUp,
            IpcCommand::CycleWidthDown,
            IpcCommand::CycleHeightUp,
            IpcCommand::CycleHeightDown,
            IpcCommand::EqualizeColumnHeights,
            IpcCommand::QueryStatus,
            IpcCommand::HealthCheck,
        ];

        for cmd in commands {
            let json = serde_json::to_string(&cmd).expect("Failed to serialize command");
            let roundtrip: IpcCommand =
                serde_json::from_str(&json).expect("Failed to deserialize command");
            assert_eq!(cmd, roundtrip, "Roundtrip failed for {:?}", cmd);
        }
    }

    #[test]
    fn test_all_response_types_roundtrip() {
        // Verify all response variants serialize and deserialize correctly
        let responses = vec![
            IpcResponse::Ok,
            IpcResponse::Error {
                message: "Test error".to_string(),
            },
            IpcResponse::WorkspaceState {
                columns: 5,
                windows: 10,
                focused_column: 2,
                focused_window: 1,
                scroll_offset: 200.0,
                total_width: 4000,
            },
            IpcResponse::FocusedWindow {
                window_id: Some(12345),
                column_index: 1,
                window_index: 0,
            },
            IpcResponse::FocusedWindow {
                window_id: None,
                column_index: 0,
                window_index: 0,
            },
            IpcResponse::WindowList {
                windows: vec![WindowInfo {
                    window_id: 1,
                    title: "Test Window".to_string(),
                    class_name: "TestClass".to_string(),
                    process_id: 100,
                    executable: "test.exe".to_string(),
                    rect: IpcRect::new(0, 0, 800, 600),
                    column_index: Some(0),
                    window_index: Some(0),
                    monitor_id: 1,
                    is_floating: false,
                    is_focused: true,
                }],
            },
            IpcResponse::WindowList { windows: vec![] },
            IpcResponse::FocusedWindowInfo {
                window: Some(WindowInfo {
                    window_id: 42,
                    title: "Focused".to_string(),
                    class_name: "FocusedClass".to_string(),
                    process_id: 200,
                    executable: "focused.exe".to_string(),
                    rect: IpcRect::new(100, 100, 1024, 768),
                    column_index: Some(1),
                    window_index: Some(0),
                    monitor_id: 2,
                    is_floating: false,
                    is_focused: true,
                }),
            },
            IpcResponse::FocusedWindowInfo { window: None },
            IpcResponse::StatusInfo {
                version: "0.1.0".to_string(),
                monitors: 2,
                total_windows: 5,
                uptime_seconds: 3600,
            },
            IpcResponse::HealthInfo {
                healthy: true,
                uptime_seconds: 120,
                total_windows: 3,
                monitors: 1,
                paused: false,
            },
        ];

        for resp in responses {
            let json = serde_json::to_string(&resp).expect("Failed to serialize response");
            let roundtrip: IpcResponse =
                serde_json::from_str(&json).expect("Failed to deserialize response");
            assert_eq!(resp, roundtrip, "Roundtrip failed for {:?}", resp);
        }
    }

    #[test]
    fn test_window_info_serialization() {
        let info = WindowInfo {
            window_id: 12345,
            title: "Test Window".to_string(),
            class_name: "TestClass".to_string(),
            process_id: 1234,
            executable: "test.exe".to_string(),
            rect: IpcRect::new(100, 100, 800, 600),
            column_index: Some(0),
            window_index: Some(0),
            monitor_id: 1,
            is_floating: false,
            is_focused: true,
        };

        let json = serde_json::to_string(&info).unwrap();
        let roundtrip: WindowInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, roundtrip);
    }

    #[test]
    fn test_query_all_windows_command() {
        let cmd = IpcCommand::QueryAllWindows;
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("query_all_windows"));

        let roundtrip: IpcCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, roundtrip);
    }

    #[test]
    fn test_window_list_response() {
        let resp = IpcResponse::WindowList {
            windows: vec![WindowInfo {
                window_id: 1,
                title: "Window 1".to_string(),
                class_name: "Class1".to_string(),
                process_id: 100,
                executable: "app.exe".to_string(),
                rect: IpcRect::new(0, 0, 800, 600),
                column_index: Some(0),
                window_index: Some(0),
                monitor_id: 1,
                is_floating: false,
                is_focused: true,
            }],
        };

        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("window_list"));

        let roundtrip: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, roundtrip);
    }

    #[test]
    fn test_line_delimited_protocol() {
        // Simulate the actual IPC protocol: JSON + newline
        let cmd = IpcCommand::QueryWorkspace;
        let wire_format = serde_json::to_string(&cmd).unwrap() + "\n";

        // Parse as if reading from pipe (trim newline)
        let parsed: IpcCommand = serde_json::from_str(wire_format.trim()).unwrap();
        assert_eq!(cmd, parsed);

        // Same for response
        let resp = IpcResponse::WorkspaceState {
            columns: 2,
            windows: 3,
            focused_column: 0,
            focused_window: 0,
            scroll_offset: 0.0,
            total_width: 1600,
        };
        let wire_format = serde_json::to_string(&resp).unwrap() + "\n";
        let parsed: IpcResponse = serde_json::from_str(wire_format.trim()).unwrap();
        assert_eq!(resp, parsed);
    }

    #[test]
    fn test_invalid_json_handling() {
        // Verify that invalid JSON produces clear errors
        let result: Result<IpcCommand, _> = serde_json::from_str("not valid json");
        assert!(result.is_err());

        let result: Result<IpcCommand, _> = serde_json::from_str("{\"type\": \"unknown_command\"}");
        assert!(result.is_err());

        let result: Result<IpcResponse, _> = serde_json::from_str("{\"status\": \"invalid\"}");
        assert!(matches!(result, Ok(IpcResponse::Unknown)));
    }

    #[test]
    fn test_pipe_name_format() {
        // Verify pipe name follows Windows named pipe convention
        assert!(PIPE_NAME.starts_with(r"\\.\pipe\"));
        assert_eq!(PIPE_NAME, r"\\.\pipe\leopardwm");
    }

    #[test]
    fn test_scoped_pipe_name_for_user_sanitizes_and_scopes() {
        let scoped = scoped_pipe_name_for_user("ACME\\Alice Example!");
        assert!(scoped.starts_with(PIPE_NAME));
        assert_ne!(scoped, PIPE_NAME);
        assert!(scoped.ends_with("acme_alice_example"));
    }

    #[test]
    fn test_pipe_name_candidates_include_legacy_fallback() {
        let candidates = pipe_name_candidates();
        assert!(!candidates.is_empty());
        assert_eq!(candidates.last().unwrap(), PIPE_NAME);
    }

    #[test]
    fn test_protocol_version_helpers() {
        assert_eq!(protocol_id(), IPC_PROTOCOL_VERSION);
        assert!(is_protocol_version_supported(IPC_PROTOCOL_VERSION));
        assert!(!is_protocol_version_supported(0));
        assert!(!is_protocol_version_supported(IPC_PROTOCOL_VERSION + 1));
    }

    #[test]
    fn test_max_message_size_defined() {
        const { assert!(MAX_IPC_MESSAGE_SIZE > 0) };
    }

    #[test]
    fn test_max_message_size_reasonable() {
        // Between 1 KiB and 1 MiB
        const { assert!(MAX_IPC_MESSAGE_SIZE >= 1024) };
        const { assert!(MAX_IPC_MESSAGE_SIZE <= 1024 * 1024) };
    }
}
