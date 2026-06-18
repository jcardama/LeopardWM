//! Clap argument definitions: CLI struct, subcommands, and direction enums.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub(crate) const RUN_WAIT_DEFAULT_MS: u64 = 5000;
const MIN_SET_WIDTH_FRACTION: f64 = 0.1;
const MAX_SET_WIDTH_FRACTION: f64 = 1.0;

pub(crate) fn validate_set_width_fraction(fraction: f64) -> std::result::Result<(), String> {
    if !fraction.is_finite() {
        return Err("set-width fraction must be a finite number".to_string());
    }
    if !(MIN_SET_WIDTH_FRACTION..=MAX_SET_WIDTH_FRACTION).contains(&fraction) {
        return Err(format!(
            "set-width fraction must be in [{:.1}, {:.1}]",
            MIN_SET_WIDTH_FRACTION, MAX_SET_WIDTH_FRACTION
        ));
    }
    Ok(())
}

pub(crate) fn parse_set_width_fraction(raw: &str) -> std::result::Result<f64, String> {
    let fraction = raw
        .parse::<f64>()
        .map_err(|_| format!("Invalid set-width fraction '{}': expected a number", raw))?;
    validate_set_width_fraction(fraction)?;
    Ok(fraction)
}

#[derive(Parser)]
#[command(name = "leopardwm-cli")]
#[command(author, version, about = "Control the LeopardWM window manager")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// Focus commands
    Focus {
        #[command(subcommand)]
        direction: FocusDirection,
    },
    /// Scroll the viewport
    Scroll {
        #[command(subcommand)]
        direction: ScrollDirection,
    },
    /// Move the focused column left or right
    Move {
        #[command(subcommand)]
        direction: MoveDirection,
    },
    /// Move the focused window in any direction
    MoveWindow {
        #[command(subcommand)]
        direction: MoveWindowDirection,
    },
    /// Expel the focused window to a new column
    Expel {
        #[command(subcommand)]
        direction: ExpelDirection,
    },
    /// Consume the adjacent column's window into the focused column
    Consume {
        #[command(subcommand)]
        direction: ConsumeDirection,
    },
    /// Resize the focused column
    Resize {
        /// Width delta in pixels (positive to grow, negative to shrink)
        #[arg(short, long)]
        delta: i32,
    },
    /// Focus a different monitor
    FocusMonitor {
        #[command(subcommand)]
        direction: MonitorDirection,
    },
    /// Move the focused window to a different monitor
    MoveToMonitor {
        #[command(subcommand)]
        direction: MonitorDirection,
    },
    /// Query workspace state
    Query {
        #[command(subcommand)]
        what: QueryType,
    },
    /// Re-enumerate windows
    Refresh,
    /// Reload configuration from file
    Reload,
    /// Start daemon (if needed) and apply layout once
    Run {
        /// Skip applying layout after the daemon is ready
        #[arg(long)]
        no_apply: bool,
        /// How long to wait for the daemon to become ready (milliseconds)
        #[arg(long, default_value_t = RUN_WAIT_DEFAULT_MS)]
        wait_ms: u64,
        /// Start daemon in safe mode (no hotkeys, no cloaking)
        #[arg(long)]
        safe_mode: bool,
        /// Spawn the daemon directly without the watchdog supervisor (useful
        /// for development/debugging — disables crash recovery + auto-restart).
        #[arg(long)]
        no_watchdog: bool,
    },
    /// Stop the daemon
    Stop,
    /// Subscribe to LeopardWM state changes (newline-delimited JSON to stdout)
    ///
    /// Pipe into `jq` for pretty output, or wire into a status bar to
    /// re-render on each event. Default is to receive every event kind;
    /// `--events workspace,focused_window` filters at the daemon level.
    /// Press Ctrl+C to disconnect.
    Subscribe {
        /// Comma-separated event kinds: workspace, focused_window, layout, config, heartbeat
        #[arg(long, value_delimiter = ',')]
        events: Option<Vec<String>>,
    },
    /// Toggle pause/resume of tiling operations
    #[command(visible_alias = "pause")]
    TogglePause,
    /// Enable, disable, or check the swap-chain ghost-animation feature
    Ghost {
        #[command(subcommand)]
        action: GhostAction,
    },
    /// Emergency recovery command to revert managed windows
    #[command(visible_alias = "recover")]
    PanicRevert,
    /// Local emergency restore that bypasses daemon IPC
    #[command(visible_alias = "restore-windows")]
    EmergencyUncloak,
    /// Close the focused window
    CloseWindow,
    /// Toggle floating for the focused window
    ToggleFloating,
    /// Toggle fullscreen for the focused window
    ToggleFullscreen,
    /// Designate the focused window as the scratchpad and hide it
    ScratchpadStash,
    /// Show the scratchpad (floating, centered) if hidden, or hide it
    ScratchpadToggle,
    /// Toggle sticky (pinned visible on every workspace) for the focused window
    ToggleSticky,
    /// Toggle where new windows open: their own new column or stacked into the focused column
    ToggleNewWindowPlacement,
    /// Toggle tabbed mode on the focused column (niri-style: only the
    /// active tab is visible, with a tab strip overlay above the column)
    ToggleTabbed,
    /// Set the focused column width
    SetWidth {
        /// Width as fraction of viewport (e.g., 0.333, 0.5, 0.667)
        #[arg(short, long, value_parser = parse_set_width_fraction)]
        fraction: f64,
    },
    /// Center the focused column in the viewport
    CenterColumn,
    /// Maximize the focused column to fill the viewport width
    MaximizeColumn,
    /// Equalize all column widths
    EqualizeWidths,
    /// Cycle focused column width up through presets
    CycleWidthUp,
    /// Cycle focused column width down through presets
    CycleWidthDown,
    /// Cycle focused window height up through presets
    CycleHeightUp,
    /// Cycle focused window height down through presets
    CycleHeightDown,
    /// Equalize window heights in the focused column
    EqualizeHeights,
    /// Switch to workspace N (1-9)
    Workspace {
        /// Workspace number (1-9)
        #[arg(value_parser = clap::value_parser!(u8).range(1..=9))]
        number: u8,
    },
    /// Move the focused window to workspace N (1-9)
    MoveToWorkspace {
        /// Workspace number (1-9)
        #[arg(value_parser = clap::value_parser!(u8).range(1..=9))]
        number: u8,
    },
    /// Switch to the next workspace (wraps 9 -> 1)
    WorkspaceNext,
    /// Switch to the previous workspace (wraps 1 -> 9)
    WorkspacePrev,
    /// Toggle the workspace overview (map of non-empty workspaces)
    ToggleOverview,
    /// Query daemon status
    Status,
    /// Run diagnostic checks
    Doctor,
    /// Manage auto-start on login
    Autostart {
        #[command(subcommand)]
        action: AutostartAction,
    },
    /// Collect diagnostic logs for bug reports
    CollectLogs,
    /// Manage configuration files
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
pub(crate) enum FocusDirection {
    /// Focus the column to the left
    Left,
    /// Focus the column to the right
    Right,
    /// Focus the window above (in stacked columns)
    Up,
    /// Focus the window below (in stacked columns)
    Down,
    /// Focus the first (leftmost) column of the strip
    Start,
    /// Focus the last (rightmost) column of the strip
    End,
}

#[derive(Subcommand)]
pub(crate) enum ScrollDirection {
    /// Scroll viewport left
    Left {
        /// Pixels to scroll (default: 100)
        #[arg(short, long, default_value = "100")]
        pixels: i32,
    },
    /// Scroll viewport right
    Right {
        /// Pixels to scroll (default: 100)
        #[arg(short, long, default_value = "100")]
        pixels: i32,
    },
}

#[derive(Subcommand)]
pub(crate) enum MoveDirection {
    /// Move focused column left
    Left,
    /// Move focused column right
    Right,
    /// Move focused column to the start (leftmost) of the strip
    Start,
    /// Move focused column to the end (rightmost) of the strip
    End,
}

#[derive(Subcommand)]
pub(crate) enum MoveWindowDirection {
    /// Move focused window to the column on the left
    Left,
    /// Move focused window to the column on the right
    Right,
    /// Move focused window up in column
    Up,
    /// Move focused window down in column
    Down,
}

#[derive(Subcommand)]
pub(crate) enum ExpelDirection {
    /// Expel to a new column on the left
    Left,
    /// Expel to a new column on the right
    Right,
}

#[derive(Subcommand, Debug)]
pub(crate) enum ConsumeDirection {
    /// Pull the left column's window into the focused column
    Left,
    /// Pull the right column's window into the focused column
    Right,
}

#[derive(Subcommand)]
pub(crate) enum MonitorDirection {
    /// Focus/move to the monitor on the left
    Left,
    /// Focus/move to the monitor on the right
    Right,
}

#[derive(Subcommand)]
pub(crate) enum QueryType {
    /// Get current workspace state
    Workspace,
    /// Get focused window info
    Focused,
    /// List all managed windows
    All,
}

#[derive(Subcommand)]
pub(crate) enum AutostartAction {
    /// Enable auto-start on login
    Enable,
    /// Disable auto-start on login
    Disable,
}

#[derive(Subcommand)]
pub(crate) enum GhostAction {
    /// Turn on swap-chain ghost animation
    Enable,
    /// Turn off swap-chain ghost animation
    Disable,
    /// Report the current state
    Status,
}

#[derive(Subcommand)]
pub(crate) enum ConfigAction {
    /// Generate default configuration file
    Init {
        /// Output path (default: %APPDATA%/leopardwm/config/config.toml)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Overwrite existing config file
        #[arg(short, long)]
        force: bool,
        /// Use a preset profile: developer, laptop, ultrawide
        #[arg(short, long)]
        profile: Option<String>,
    },
    /// Reset config to defaults (backs up current to config.toml.bak)
    Reset,
    /// Create a backup of the current config
    Backup,
    /// Restore config from backup
    Restore,
}
