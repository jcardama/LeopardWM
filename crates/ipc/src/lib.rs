//! LeopardWM IPC Protocol
//!
//! Shared types for daemon-CLI communication over Windows named pipes.

use serde::{Deserialize, Serialize};

fn default_active_workspace() -> u8 {
    1
}

/// Named pipe path for IPC communication.
pub const PIPE_NAME: &str = r"\\.\pipe\leopardwm";
/// Maximum length of sanitized user scope appended to the pipe path.
const MAX_PIPE_SCOPE_SEGMENT_LEN: usize = 64;

/// IPC protocol version for lightweight compatibility checks.
///
/// History:
/// - v1: initial (workspace ops, focus ops, queries, pub/sub Subscribe).
/// - v2: tabbed columns — `ColumnSummary.mode` extension on `LayoutChanged`,
///   new `ToggleTabbed` and `SetActiveTab` commands. Wire is additive;
///   subscribers using `serde(default)` parse v1 payloads cleanly.
pub const IPC_PROTOCOL_VERSION: u32 = 2;
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

/// Kinds of events a subscriber can receive. Used as the `Subscribe`
/// command's filter set and to dedup which event variants flow to which
/// client.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Workspace switches (`WorkspaceChanged`).
    Workspace,
    /// Focused window changes (`FocusedWindowChanged`).
    FocusedWindow,
    /// Layout structure changes (`LayoutChanged`). Also fires once on
    /// subscribe as part of the initial snapshot.
    Layout,
    /// Configuration reloads (`ConfigReloaded`).
    Config,
    /// Periodic liveness heartbeats (`Heartbeat`). Subscribers receive
    /// one every 30s of silence so they can detect dead daemon pipes.
    Heartbeat,
}

impl EventKind {
    /// All event kinds, useful as a default subscription set.
    pub fn all() -> std::collections::BTreeSet<EventKind> {
        [
            EventKind::Workspace,
            EventKind::FocusedWindow,
            EventKind::Layout,
            EventKind::Config,
            EventKind::Heartbeat,
        ]
        .into_iter()
        .collect()
    }
}

/// Display mode for a column in a `LayoutChanged` event payload.
///
/// Mirrors `core_layout::ColumnMode`. Vertical (default) renders all
/// non-minimized windows stacked top-to-bottom. Tabbed renders only the
/// window at `active_idx` filling the column rect; bars should render a
/// tab strip listing all `window_ids`.
///
/// Wire: tagged `{"type": "vertical"}` or `{"type": "tabbed", "active_idx": N}`.
/// `#[serde(default)]` on the parent `ColumnSummary.mode` field means v1
/// payloads (no `mode` key) deserialize as `Vertical` for forward compat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ColumnSummaryMode {
    #[default]
    Vertical,
    Tabbed { active_idx: usize },
}

/// One column entry in a `LayoutChanged` event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnSummary {
    /// Window IDs in the column, top to bottom (or tab order, in Tabbed mode).
    pub window_ids: Vec<u64>,
    /// Column width in pixels (intrinsic strip width, monitor-independent).
    pub width_px: i32,
    /// Per-window height weights (parallel to `window_ids`, sums to ~1.0).
    /// Empty means equal distribution. Populated so bars rendering
    /// stacked windows can show vertical-split changes (cycle-height,
    /// equalize-heights) without a follow-up query. Ignored in Tabbed mode
    /// since only one window is visible at a time.
    #[serde(default)]
    pub height_weights: Vec<f64>,
    /// Display mode for this column. Added in protocol v2; defaults to
    /// `Vertical` so v1 payloads deserialize cleanly.
    #[serde(default)]
    pub mode: ColumnSummaryMode,
}

/// Events streamed to subscribers after a `Subscribe` command. Each frame
/// is one JSON object on the wire (newline-terminated, same framing as
/// `IpcResponse`). After a client receives `IpcResponse::Subscribed`, all
/// subsequent frames on that pipe deserialize as `IpcEvent`, NOT
/// `IpcResponse` — the two types share the wire format but have
/// incompatible serde tags (`type` vs `status`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcEvent {
    /// The active workspace on a monitor changed.
    WorkspaceChanged {
        /// Monitor ID where the workspace switch happened.
        monitor: i64,
        /// Previous workspace index (0-based; CLI displays 1-based).
        old_index: u8,
        /// New workspace index (0-based).
        new_index: u8,
        /// Display name of the new workspace, or `None` if unnamed.
        /// Lets bars render a label instead of a bare number.
        #[serde(default)]
        name: Option<String>,
    },
    /// The focused window changed (or was cleared). Title/class/exec
    /// fields are best-effort enrichment and may be `None` if the window
    /// died between the focus event and the lookup.
    FocusedWindowChanged {
        /// Monitor ID of the focused window (or where focus was last seen).
        monitor: i64,
        /// HWND of the focused window, or `None` if focus was cleared.
        hwnd: Option<u64>,
        /// Window title, if obtainable.
        title: Option<String>,
        /// Window class name, if obtainable.
        class_name: Option<String>,
        /// Process executable path, if obtainable.
        executable: Option<String>,
    },
    /// The structurally-distinct layout for the focused workspace
    /// settled. Carries column structure inline so subscribers can render
    /// without a follow-up `QueryWorkspace`.
    LayoutChanged {
        /// Monitor ID whose layout settled.
        monitor: i64,
        /// Workspace index that settled.
        workspace_index: u8,
        /// Index of the focused column, or `None` if no column is focused.
        focused_column: Option<usize>,
        /// Columns in left-to-right order.
        columns: Vec<ColumnSummary>,
    },
    /// `lwm reload` completed (config reread, rules recompiled, layout reapplied).
    ConfigReloaded,
    /// Periodic liveness signal. Sent after ~30s of silence on a stream
    /// so clients can detect dead daemon pipes via missing heartbeats.
    Heartbeat {
        /// Daemon uptime in seconds at the time of the heartbeat.
        uptime_seconds: u64,
    },
    /// The daemon's broadcast buffer overflowed for this subscriber and
    /// some events were dropped. Recommended recovery: reconnect with a
    /// fresh `Subscribe` to receive a new snapshot.
    Lagged {
        /// Number of events the broadcast layer dropped before delivery.
        skipped: u64,
    },
}

impl IpcEvent {
    /// The `EventKind` this variant belongs to. Used by the IPC server to
    /// filter events against a subscriber's chosen set.
    pub fn kind(&self) -> EventKind {
        match self {
            IpcEvent::WorkspaceChanged { .. } => EventKind::Workspace,
            IpcEvent::FocusedWindowChanged { .. } => EventKind::FocusedWindow,
            IpcEvent::LayoutChanged { .. } => EventKind::Layout,
            IpcEvent::ConfigReloaded => EventKind::Config,
            IpcEvent::Heartbeat { .. } => EventKind::Heartbeat,
            // Lagged is an internal control event, not subscribable; emit
            // unconditionally to any subscriber that hits the lagged
            // branch on the broadcast receiver, regardless of filter.
            IpcEvent::Lagged { .. } => EventKind::Heartbeat,
        }
    }
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
    /// Focus the next window in linear order (across columns).
    FocusNext,
    /// Focus the previous window in linear order (across columns).
    FocusPrev,

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
    /// Toggle the swap-chain ghost-animation feature at runtime.
    /// `None` queries current state; `Some(b)` sets it. Returns
    /// `BoolValue` with the new (or current) state.
    SetGhostAnimation { enabled: Option<bool> },

    /// Query detailed information about all managed windows.
    QueryAllWindows,

    /// Close the focused window.
    CloseWindow,
    /// Toggle floating state for the focused window.
    ToggleFloating,
    /// Toggle fullscreen for the focused window.
    ToggleFullscreen,
    /// Designate the focused window as the scratchpad and hide it.
    ScratchpadStash,
    /// Show the scratchpad window (floating, centered) if hidden, or hide
    /// it if shown.
    ScratchpadToggle,
    /// Set the focused column width as a fraction of the viewport.
    SetColumnWidth {
        /// Fraction of viewport width (e.g., 0.333, 0.5, 0.667).
        fraction: f64,
    },
    /// Center the focused column in the viewport.
    CenterColumn,
    /// Toggle the focused column to fill the viewport width.
    MaximizeColumn,
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

    /// Switch to workspace N (1-9) on the focused monitor.
    SwitchWorkspace {
        /// Workspace number (1-9).
        index: u8,
    },
    /// Move the focused window to workspace N (1-9) on the focused monitor.
    MoveToWorkspace {
        /// Workspace number (1-9).
        index: u8,
    },
    /// Switch to the previous workspace (cycles 1 → 9 on wrap).
    WorkspacePrev,
    /// Switch to the next workspace (cycles 9 → 1 on wrap).
    WorkspaceNext,

    /// Query whether the daemon is configured to auto-start with Windows.
    GetAutoStart,
    /// Enable or disable auto-start with Windows (writes the HKCU Run key).
    SetAutoStart {
        /// Whether auto-start should be enabled.
        enabled: bool,
    },

    /// Switch the connection into stream mode and start receiving
    /// `IpcEvent` frames as state changes occur. After the daemon
    /// responds with `IpcResponse::Subscribed`, the pipe stays open and
    /// each subsequent frame is an `IpcEvent` (not an `IpcResponse`). The
    /// pipe cannot be used for further commands; clients open a second
    /// pipe for ad-hoc queries while subscribed.
    Subscribe {
        /// Event kinds to receive. An empty set is treated as "all kinds".
        events: std::collections::BTreeSet<EventKind>,
    },

    /// Toggle the focused column between Vertical (stacked) and Tabbed
    /// display modes. Entering Tabbed seeds `active_idx` with the focused
    /// window. No-op if the focused column has fewer than 2 windows.
    /// Added in protocol v2.
    ToggleTabbed,

    /// Set the active tab on a Tabbed column. Used internally by the tab
    /// strip overlay's click handler to translate `WM_LBUTTONDOWN` into a
    /// real focus change. Sets the column's `active_idx` AND, when the
    /// target column is the focused column, also moves
    /// `focused_window_in_column` so the dependent invariants (border
    /// placement, `QueryFocused`, `sync_foreground_window`) flow through.
    /// Added in protocol v2.
    SetActiveTab {
        /// Column index (0-based).
        column: usize,
        /// Tab index within the column (0-based).
        tab: usize,
    },
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
        /// Active workspace number (1-based).
        #[serde(default = "default_active_workspace")]
        active_workspace: u8,
        /// Display name of the active workspace, or `None` if unnamed.
        #[serde(default)]
        active_workspace_name: Option<String>,
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
    /// Auto-start state response.
    AutoStartState {
        /// Whether auto-start is currently enabled.
        enabled: bool,
    },
    /// Generic boolean state response (e.g., `SetGhostAnimation`).
    BoolValue {
        /// The state being reported.
        value: bool,
    },
    /// Acknowledgment for `IpcCommand::Subscribe`. After the client reads
    /// this response, it must switch its frame parser from `IpcResponse`
    /// to `IpcEvent` for all subsequent reads on this pipe.
    Subscribed {
        /// Echoed event-kind set the daemon will deliver (matches the
        /// requested set after defaulting and validation).
        events: std::collections::BTreeSet<EventKind>,
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
        /// Live DWM thumbnail registrations for the swap-chain ghost
        /// animation. Should be 0 at rest; a non-zero value while no
        /// animation is in flight indicates a handle leak.
        #[serde(default)]
        thumbnail_register_balance: i64,
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
            active_workspace: 1,
            active_workspace_name: None,
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
            IpcCommand::ScratchpadStash,
            IpcCommand::ScratchpadToggle,
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
            IpcCommand::MaximizeColumn,
            IpcCommand::SwitchWorkspace { index: 1 },
            IpcCommand::SwitchWorkspace { index: 9 },
            IpcCommand::MoveToWorkspace { index: 1 },
            IpcCommand::MoveToWorkspace { index: 5 },
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
                active_workspace: 1,
                active_workspace_name: None,
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
                thumbnail_register_balance: 0,
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
            active_workspace: 1,
            active_workspace_name: None,
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

    #[test]
    fn test_subscribe_command_round_trip() {
        let mut events = std::collections::BTreeSet::new();
        events.insert(EventKind::Workspace);
        events.insert(EventKind::FocusedWindow);
        let cmd = IpcCommand::Subscribe { events };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("subscribe"));
        assert!(json.contains("workspace"));
        assert!(json.contains("focused_window"));
        let cmd2: IpcCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, cmd2);
    }

    #[test]
    fn test_subscribed_response_round_trip() {
        let resp = IpcResponse::Subscribed { events: EventKind::all() };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("subscribed"));
        let resp2: IpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, resp2);
    }

    #[test]
    fn test_event_workspace_changed_round_trip() {
        let ev = IpcEvent::WorkspaceChanged {
            monitor: 12345,
            old_index: 0,
            new_index: 4,
            name: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("workspace_changed"));
        let ev2: IpcEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, ev2);
    }

    #[test]
    fn test_event_focused_window_changed_round_trip() {
        let ev = IpcEvent::FocusedWindowChanged {
            monitor: 1,
            hwnd: Some(0xABCD),
            title: Some("Notepad".to_string()),
            class_name: Some("Notepad".to_string()),
            executable: Some("C:\\Windows\\notepad.exe".to_string()),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let ev2: IpcEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, ev2);

        let cleared = IpcEvent::FocusedWindowChanged {
            monitor: 1,
            hwnd: None,
            title: None,
            class_name: None,
            executable: None,
        };
        let json = serde_json::to_string(&cleared).unwrap();
        let cleared2: IpcEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(cleared, cleared2);
    }

    #[test]
    fn test_event_layout_changed_round_trip() {
        let ev = IpcEvent::LayoutChanged {
            monitor: 1,
            workspace_index: 2,
            focused_column: Some(1),
            columns: vec![
                ColumnSummary {
                    window_ids: vec![100, 200],
                    width_px: 800,
                    height_weights: vec![0.6, 0.4],
                    mode: ColumnSummaryMode::default(),
                },
                ColumnSummary {
                    window_ids: vec![300],
                    width_px: 600,
                    height_weights: vec![1.0],
                    mode: ColumnSummaryMode::default(),
                },
            ],
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("layout_changed"));
        let ev2: IpcEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, ev2);
    }

    #[test]
    fn test_event_layout_changed_with_tabbed_column_round_trip() {
        let ev = IpcEvent::LayoutChanged {
            monitor: 1,
            workspace_index: 2,
            focused_column: Some(0),
            columns: vec![ColumnSummary {
                window_ids: vec![100, 200, 300],
                width_px: 800,
                height_weights: Vec::new(),
                mode: ColumnSummaryMode::Tabbed { active_idx: 1 },
            }],
        };
        let json = serde_json::to_string(&ev).unwrap();
        // Tag-based representation; subscribers branch on `mode.type`.
        assert!(json.contains("\"mode\":{\"type\":\"tabbed\""));
        assert!(json.contains("\"active_idx\":1"));
        let ev2: IpcEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, ev2);
    }

    #[test]
    fn test_layout_changed_v1_payload_backward_compat() {
        // A v1-shape `LayoutChanged` (no `mode` field on columns) must
        // deserialize into the new struct with `mode = Vertical` so old
        // daemons / cached payloads stay compatible.
        let v1 = r#"{
            "type": "layout_changed",
            "monitor": 1,
            "workspace_index": 0,
            "focused_column": 0,
            "columns": [
                {"window_ids": [100, 200], "width_px": 800, "height_weights": [0.5, 0.5]}
            ]
        }"#;
        let ev: IpcEvent = serde_json::from_str(v1).unwrap();
        if let IpcEvent::LayoutChanged { columns, .. } = ev {
            assert_eq!(columns.len(), 1);
            assert!(matches!(columns[0].mode, ColumnSummaryMode::Vertical));
        } else {
            panic!("expected LayoutChanged");
        }
    }

    #[test]
    fn test_toggle_tabbed_command_round_trip() {
        let cmd = IpcCommand::ToggleTabbed;
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#"{"type":"toggle_tabbed"}"#);
        let cmd2: IpcCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, cmd2);
    }

    #[test]
    fn test_set_active_tab_command_round_trip() {
        let cmd = IpcCommand::SetActiveTab { column: 2, tab: 1 };
        let json = serde_json::to_string(&cmd).unwrap();
        let cmd2: IpcCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, cmd2);
    }

    #[test]
    fn test_protocol_version_bumped_to_v2() {
        // Sanity guard: bumping the version forces a deliberate review of
        // wire-compat docs in agent_docs/ipc-events.md when this test breaks.
        assert_eq!(IPC_PROTOCOL_VERSION, 2);
        // Old v1 clients should still negotiate.
        assert!(is_protocol_version_supported(1));
        assert!(is_protocol_version_supported(2));
    }

    #[test]
    fn test_event_config_reloaded_round_trip() {
        let ev = IpcEvent::ConfigReloaded;
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(json, r#"{"type":"config_reloaded"}"#);
        let ev2: IpcEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, ev2);
    }

    #[test]
    fn test_event_heartbeat_round_trip() {
        let ev = IpcEvent::Heartbeat { uptime_seconds: 12345 };
        let json = serde_json::to_string(&ev).unwrap();
        let ev2: IpcEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, ev2);
    }

    #[test]
    fn test_event_lagged_round_trip() {
        let ev = IpcEvent::Lagged { skipped: 42 };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("lagged"));
        let ev2: IpcEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, ev2);
    }

    #[test]
    fn test_event_kind_filtering() {
        assert_eq!(
            IpcEvent::WorkspaceChanged { monitor: 1, old_index: 0, new_index: 1, name: None }.kind(),
            EventKind::Workspace
        );
        assert_eq!(IpcEvent::ConfigReloaded.kind(), EventKind::Config);
        assert_eq!(IpcEvent::Heartbeat { uptime_seconds: 0 }.kind(), EventKind::Heartbeat);
    }

    #[test]
    fn test_event_kind_all_contains_every_variant() {
        let all = EventKind::all();
        assert!(all.contains(&EventKind::Workspace));
        assert!(all.contains(&EventKind::FocusedWindow));
        assert!(all.contains(&EventKind::Layout));
        assert!(all.contains(&EventKind::Config));
        assert!(all.contains(&EventKind::Heartbeat));
        assert_eq!(all.len(), 5);
    }
}
