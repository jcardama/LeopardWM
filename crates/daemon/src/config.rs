//! Configuration management for LeopardWM daemon.
//!
//! Configuration is loaded from TOML files in the following locations (in order):
//! 1. `%APPDATA%/leopardwm/config.toml` (Windows standard)
//! 2. `~/.config/leopardwm/config.toml` (Unix-style, for WSL compatibility)
//! 3. `./config.toml` (current directory, for development)

use anyhow::{Context, Result};
use directories::ProjectDirs;
use leopardwm_core_layout::CenteringMode;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

/// Executables whose windows should never be tiled (system dialogs, security prompts, etc.).
/// These are appended as built-in Ignore rules after user-defined rules.
const BUILTIN_IGNORE_EXECUTABLES: &[&str] = &[
    "smartscreen.exe",        // Windows Defender SmartScreen
    "consent.exe",            // UAC elevation prompt
    "msiexec.exe",            // Windows Installer
    "CredentialUIBroker.exe", // Windows credential/login prompt
    "SnippingTool.exe",       // Screen capture overlay — breaks when repositioned
];

/// Main configuration structure for LeopardWM.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Layout configuration.
    pub layout: LayoutConfig,
    /// Appearance configuration.
    pub appearance: AppearanceConfig,
    /// Behavior configuration.
    pub behavior: BehaviorConfig,
    /// Hotkey bindings.
    pub hotkeys: HotkeyConfig,
    /// Window rules for per-window behavior.
    #[serde(default)]
    pub window_rules: Vec<WindowRule>,
    /// Gesture bindings for touchpad support.
    #[serde(default)]
    pub gestures: GestureConfig,
    /// Snap hint configuration.
    #[serde(default)]
    pub snap_hints: SnapHintConfig,
}

/// Layout-related configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutConfig {
    /// Gap between columns in pixels.
    #[serde(default = "default_gap")]
    pub gap: i32,

    /// Outer gap at the left edge of the viewport.
    #[serde(default = "default_outer_gap")]
    pub outer_gap_left: i32,

    /// Outer gap at the right edge of the viewport.
    #[serde(default = "default_outer_gap")]
    pub outer_gap_right: i32,

    /// Outer gap at the top edge of the viewport.
    #[serde(default = "default_outer_gap")]
    pub outer_gap_top: i32,

    /// Outer gap at the bottom edge of the viewport.
    #[serde(default = "default_outer_gap")]
    pub outer_gap_bottom: i32,

    /// Centering mode for focus navigation.
    #[serde(default)]
    pub centering_mode: CenteringModeConfig,

    /// Width presets for cycling (fractions of usable viewport width).
    /// The first preset is also used as the default width for new columns.
    #[serde(default = "default_width_presets")]
    pub width_presets: Vec<f64>,

    /// Height presets for cycling (fractions of column height / weight).
    #[serde(default = "default_height_presets")]
    pub height_presets: Vec<f64>,

    // Legacy fields kept for backward-compatible deserialization; not used.
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    outer_gap: Option<i32>,
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    default_column_width: Option<i32>,
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    min_column_width: Option<i32>,
    #[serde(default, skip_serializing)]
    #[allow(dead_code)]
    max_column_width: Option<i32>,
}

fn default_width_presets() -> Vec<f64> {
    vec![0.333, 0.5, 0.667]
}

fn default_height_presets() -> Vec<f64> {
    vec![0.333, 0.5, 0.667]
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            gap: default_gap(),
            outer_gap_left: default_outer_gap(),
            outer_gap_right: default_outer_gap(),
            outer_gap_top: default_outer_gap(),
            outer_gap_bottom: default_outer_gap(),
            centering_mode: CenteringModeConfig::default(),
            width_presets: default_width_presets(),
            height_presets: default_height_presets(),
            outer_gap: None,
            default_column_width: None,
            min_column_width: None,
            max_column_width: None,
        }
    }
}

impl LayoutConfig {
    /// Compute the default column width in pixels for a given viewport width,
    /// using the first width preset as a fraction.
    /// Formula: `width = fraction * (viewport - OL - OR + gap) - gap`
    /// This is independent of column count — same result whether 1 or 10 columns.
    pub fn default_column_width_px(&self, viewport_width: i32) -> i32 {
        let base = viewport_width
            .saturating_sub(self.outer_gap_left.max(0))
            .saturating_sub(self.outer_gap_right.max(0))
            .saturating_add(self.gap.max(0));
        let gap = self.gap.max(0);
        let frac = self.width_presets.first().copied().unwrap_or(0.5);
        (base as f64 * frac - gap as f64).floor().max(100.0) as i32
    }

    /// Migrate legacy `outer_gap` field to per-side fields if present.
    /// Called after deserialization.
    pub fn migrate_outer_gap(&mut self) {
        if let Some(og) = self.outer_gap.take() {
            let og = og.max(0);
            // Only migrate if the new fields are still at defaults, meaning
            // the user's config only had the old `outer_gap` key.
            let d = default_outer_gap();
            if self.outer_gap_left == d
                && self.outer_gap_right == d
                && self.outer_gap_top == d
                && self.outer_gap_bottom == d
            {
                self.outer_gap_left = og;
                self.outer_gap_right = og;
                self.outer_gap_top = og;
                self.outer_gap_bottom = og;
            }
        }
    }
}

/// Centering mode configuration (wrapper for serialization).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CenteringModeConfig {
    /// Center the focused column in the viewport.
    #[default]
    Center,
    /// Only scroll if the focused column would be outside the viewport.
    JustInView,
}

impl From<CenteringModeConfig> for CenteringMode {
    fn from(config: CenteringModeConfig) -> Self {
        match config {
            CenteringModeConfig::Center => CenteringMode::Center,
            CenteringModeConfig::JustInView => CenteringMode::JustInView,
        }
    }
}

/// Appearance-related configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    /// Whether to highlight the active window border (Windows 11+).
    #[serde(default = "default_true")]
    pub active_border: bool,

    /// Active window border color as hex RGB (e.g., "4285F4").
    #[serde(default = "default_active_border_color")]
    pub active_border_color: String,

    /// Active window border width in pixels.
    #[serde(default = "default_active_border_width")]
    pub active_border_width: u32,

    /// Active window border position: "outside" or "inside".
    #[serde(default = "default_active_border_position")]
    pub active_border_position: String,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            active_border: true,
            active_border_color: default_active_border_color(),
            active_border_width: default_active_border_width(),
            active_border_position: default_active_border_position(),
        }
    }
}

/// Behavior-related configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    /// Whether to focus new windows automatically.
    #[serde(default = "default_true")]
    pub focus_new_windows: bool,

    /// Whether to track window focus changes from Windows.
    #[serde(default = "default_true")]
    pub track_focus_changes: bool,

    /// Log level (trace, debug, info, warn, error).
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Whether focus follows the mouse cursor.
    /// When enabled, windows receive focus when the mouse enters them.
    #[serde(default = "default_false")]
    pub focus_follows_mouse: bool,

    /// Delay in milliseconds before focus changes on mouse enter.
    /// Only applies when focus_follows_mouse is true.
    #[serde(default = "default_focus_delay")]
    pub focus_follows_mouse_delay_ms: u32,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            focus_new_windows: true,
            track_focus_changes: true,
            log_level: default_log_level(),
            focus_follows_mouse: false,
            focus_follows_mouse_delay_ms: default_focus_delay(),
        }
    }
}

// Default value functions for serde
fn default_gap() -> i32 {
    10
}

fn default_outer_gap() -> i32 {
    10
}

fn default_true() -> bool {
    true
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_focus_delay() -> u32 {
    100
}

fn default_active_border_color() -> String {
    "4285F4".to_string()
}

fn default_active_border_width() -> u32 {
    2
}

fn default_active_border_position() -> String {
    "outside".to_string()
}

// ============================================================================
// Window Rules
// ============================================================================

/// A rule for per-window behavior.
///
/// Window rules are evaluated in order; the first matching rule wins.
///
/// # Example Config
///
/// ```toml
/// [[window_rules]]
/// match_class = "Chrome_WidgetWin_1"
/// match_title = ".*DevTools.*"
/// action = "float"
///
/// [[window_rules]]
/// match_executable = "spotify.exe"
/// action = "float"
///
/// [[window_rules]]
/// match_class = "#32770"  # Windows dialogs
/// action = "ignore"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowRule {
    /// Regex pattern to match window class name.
    #[serde(default)]
    pub match_class: Option<String>,

    /// Regex pattern to match window title.
    #[serde(default)]
    pub match_title: Option<String>,

    /// Executable name to match (e.g., "notepad.exe").
    #[serde(default)]
    pub match_executable: Option<String>,

    /// Action to take when the rule matches.
    #[serde(default)]
    pub action: WindowAction,

    /// Fixed width for floating windows (optional).
    #[serde(default)]
    pub width: Option<i32>,

    /// Fixed height for floating windows (optional).
    #[serde(default)]
    pub height: Option<i32>,
}

/// Action to take for a matching window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowAction {
    /// Tile the window normally (default behavior).
    #[default]
    Tile,
    /// Float the window outside the tiling layout.
    Float,
    /// Ignore the window (don't manage it at all).
    Ignore,
}

impl WindowRule {
    /// Check if this rule matches a window with the given properties.
    ///
    /// All specified match criteria must match for the rule to apply.
    /// If no match criteria are specified, the rule matches nothing.
    ///
    /// Note: Runtime code uses `CompiledWindowRule::matches()` for efficiency.
    /// This method is retained for tests and direct use.
    #[allow(dead_code)]
    pub fn matches(&self, class_name: &str, title: &str, executable: &str) -> bool {
        let has_any_criteria = self.match_class.is_some()
            || self.match_title.is_some()
            || self.match_executable.is_some();

        if !has_any_criteria {
            return false;
        }

        // Check class name if specified
        if let Some(ref pattern) = self.match_class {
            if let Ok(re) = regex::Regex::new(pattern) {
                if !re.is_match(class_name) {
                    return false;
                }
            } else {
                tracing::warn!("Invalid regex in window rule match_class: {}", pattern);
                return false;
            }
        }

        // Check title if specified
        if let Some(ref pattern) = self.match_title {
            if let Ok(re) = regex::Regex::new(pattern) {
                if !re.is_match(title) {
                    return false;
                }
            } else {
                tracing::warn!("Invalid regex in window rule match_title: {}", pattern);
                return false;
            }
        }

        // Check executable if specified (case-insensitive)
        if let Some(ref exe) = self.match_executable {
            if !executable.eq_ignore_ascii_case(exe) {
                return false;
            }
        }

        true
    }
}

/// Hotkey bindings configuration.
///
/// Each key is a hotkey string (e.g., "Win+Alt+H") and each value is a command
/// (e.g., "focus_left"). Supported commands:
/// - focus_left, focus_right, focus_up, focus_down
/// - move_column_left, move_column_right
/// - focus_monitor_left, focus_monitor_right
/// - move_to_monitor_left, move_to_monitor_right
/// - resize_grow, resize_shrink (by 50px)
/// - scroll_left, scroll_right (by 100px)
/// - refresh, reload
/// - panic_revert (emergency visibility restore + shutdown)
/// - toggle_pause (pause/resume tiling)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    /// Modifier keys required for scroll wheel navigation (e.g., "Ctrl+Alt").
    #[serde(default = "default_scroll_modifier")]
    pub scroll_modifier: String,

    /// Map of hotkey string to command name.
    #[serde(flatten)]
    pub bindings: HashMap<String, String>,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        let mut bindings = HashMap::new();

        // Navigation (focus) — Ctrl+Alt + HJKL
        bindings.insert("Ctrl+Alt+H".to_string(), "focus_left".to_string());
        bindings.insert("Ctrl+Alt+L".to_string(), "focus_right".to_string());
        bindings.insert("Ctrl+Alt+K".to_string(), "focus_up".to_string());
        bindings.insert("Ctrl+Alt+J".to_string(), "focus_down".to_string());

        // Move column — Ctrl+Alt+Shift + HL
        bindings.insert("Ctrl+Alt+Shift+H".to_string(), "move_column_left".to_string());
        bindings.insert("Ctrl+Alt+Shift+L".to_string(), "move_column_right".to_string());

        // Width cycling — Ctrl+Alt + Minus/Equals
        bindings.insert("Ctrl+Alt+Minus".to_string(), "cycle_width_down".to_string());
        bindings.insert("Ctrl+Alt+Equals".to_string(), "cycle_width_up".to_string());
        bindings.insert("Ctrl+Alt+0".to_string(), "equalize_widths".to_string());

        // Height cycling — Ctrl+Alt+Shift + Minus/Equals/0
        bindings.insert("Ctrl+Alt+Shift+Minus".to_string(), "cycle_height_down".to_string());
        bindings.insert("Ctrl+Alt+Shift+Equals".to_string(), "cycle_height_up".to_string());
        bindings.insert("Ctrl+Alt+Shift+0".to_string(), "equalize_heights".to_string());

        // Monitor focus — Ctrl+Alt+Win + Comma/Period
        bindings.insert("Ctrl+Alt+Win+Comma".to_string(), "focus_monitor_left".to_string());
        bindings.insert("Ctrl+Alt+Win+Period".to_string(), "focus_monitor_right".to_string());

        // Move to monitor — Ctrl+Alt+Win+Shift + Comma/Period
        bindings.insert(
            "Ctrl+Alt+Win+Shift+Comma".to_string(),
            "move_to_monitor_left".to_string(),
        );
        bindings.insert(
            "Ctrl+Alt+Win+Shift+Period".to_string(),
            "move_to_monitor_right".to_string(),
        );

        // Window management
        bindings.insert("Ctrl+Alt+W".to_string(), "close_window".to_string());
        bindings.insert("Ctrl+Alt+F".to_string(), "toggle_floating".to_string());
        bindings.insert("Ctrl+Alt+Shift+F".to_string(), "toggle_fullscreen".to_string());
        bindings.insert("Ctrl+Alt+P".to_string(), "toggle_pause".to_string());
        bindings.insert("Ctrl+Alt+R".to_string(), "refresh".to_string());
        bindings.insert("Ctrl+Alt+Shift+R".to_string(), "reload".to_string());

        // Move window to adjacent column — Ctrl+Alt + brackets, +Shift = expel to new column
        bindings.insert("Ctrl+Alt+Bracket_Left".to_string(), "move_window_left".to_string());
        bindings.insert("Ctrl+Alt+Bracket_Right".to_string(), "move_window_right".to_string());
        bindings.insert("Ctrl+Alt+Shift+Bracket_Left".to_string(), "expel_to_left".to_string());
        bindings.insert("Ctrl+Alt+Shift+Bracket_Right".to_string(), "expel_to_right".to_string());

        // Move window up/down in column — Ctrl+Alt+Shift + JK
        bindings.insert("Ctrl+Alt+Shift+K".to_string(), "move_window_up".to_string());
        bindings.insert("Ctrl+Alt+Shift+J".to_string(), "move_window_down".to_string());

        // Workspace switching — Ctrl+Alt + 1-9
        for i in 1..=9u8 {
            bindings.insert(format!("Ctrl+Alt+{}", i), format!("switch_workspace_{}", i));
        }

        // Move window to workspace — Ctrl+Alt+Shift + 1-9
        for i in 1..=9u8 {
            bindings.insert(format!("Ctrl+Alt+Shift+{}", i), format!("move_to_workspace_{}", i));
        }

        // Emergency escape hatch: revert visibility state and stop daemon.
        bindings.insert("Win+Ctrl+Escape".to_string(), "panic_revert".to_string());

        Self {
            scroll_modifier: default_scroll_modifier(),
            bindings,
        }
    }
}

/// Gesture bindings for touchpad support.
///
/// Maps touchpad gestures to commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GestureConfig {
    /// Whether gesture support is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Command for three-finger swipe left.
    #[serde(default = "default_swipe_left")]
    pub swipe_left: String,

    /// Command for three-finger swipe right.
    #[serde(default = "default_swipe_right")]
    pub swipe_right: String,

    /// Command for three-finger swipe up.
    #[serde(default = "default_swipe_up")]
    pub swipe_up: String,

    /// Command for three-finger swipe down.
    #[serde(default = "default_swipe_down")]
    pub swipe_down: String,

    /// Command for modifier+scroll up (physical mouse wheel).
    #[serde(default = "default_scroll_up")]
    pub scroll_up: String,

    /// Command for modifier+scroll down (physical mouse wheel).
    #[serde(default = "default_scroll_down")]
    pub scroll_down: String,
}

fn default_false() -> bool {
    false
}

fn default_swipe_left() -> String {
    "focus_left".to_string()
}

fn default_swipe_right() -> String {
    "focus_right".to_string()
}

fn default_swipe_up() -> String {
    "focus_up".to_string()
}

fn default_swipe_down() -> String {
    "focus_down".to_string()
}

fn default_scroll_up() -> String {
    "focus_next".to_string()
}

fn default_scroll_down() -> String {
    "focus_prev".to_string()
}

fn default_scroll_modifier() -> String {
    "Ctrl+Alt".to_string()
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            swipe_left: default_swipe_left(),
            swipe_right: default_swipe_right(),
            swipe_up: default_swipe_up(),
            swipe_down: default_swipe_down(),
            scroll_up: default_scroll_up(),
            scroll_down: default_scroll_down(),
        }
    }
}

/// Configuration for visual snap hints.
///
/// Snap hints provide visual feedback during resize operations,
/// showing column boundaries and snap targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapHintConfig {
    /// Whether snap hints are enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Duration to show hints in milliseconds.
    #[serde(default = "default_hint_duration")]
    pub duration_ms: u32,

    /// Opacity of the hint overlay (0-255).
    #[serde(default = "default_hint_opacity")]
    pub opacity: u8,
}

fn default_hint_duration() -> u32 {
    200
}

fn default_hint_opacity() -> u8 {
    128
}

impl Default for SnapHintConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            duration_ms: default_hint_duration(),
            opacity: default_hint_opacity(),
        }
    }
}

/// A warning generated during config validation.
#[derive(Debug, Clone)]
pub struct ConfigWarning {
    pub field: String,
    pub message: String,
}

/// A window rule with pre-compiled regex patterns for efficient matching.
#[derive(Debug, Clone)]
pub struct CompiledWindowRule {
    /// Pre-compiled regex for class name matching.
    pub class_regex: Option<regex::Regex>,
    /// Pre-compiled regex for title matching.
    pub title_regex: Option<regex::Regex>,
    /// Executable name to match (case-insensitive string comparison).
    pub match_executable: Option<String>,
    /// Action to take when the rule matches.
    pub action: WindowAction,
    /// Fixed width for floating windows (optional).
    pub width: Option<i32>,
    /// Fixed height for floating windows (optional).
    pub height: Option<i32>,
}

impl CompiledWindowRule {
    /// Check if this compiled rule matches a window.
    pub fn matches(&self, class_name: &str, title: &str, executable: &str) -> bool {
        let has_any_criteria = self.class_regex.is_some()
            || self.title_regex.is_some()
            || self.match_executable.is_some();

        if !has_any_criteria {
            return false;
        }

        if let Some(ref re) = self.class_regex {
            if !re.is_match(class_name) {
                return false;
            }
        }

        if let Some(ref re) = self.title_regex {
            if !re.is_match(title) {
                return false;
            }
        }

        if let Some(ref exe) = self.match_executable {
            if !executable.eq_ignore_ascii_case(exe) {
                return false;
            }
        }

        true
    }
}

/// Parse a command string into an IpcCommand.
///
/// Returns None if the command is not recognized.
pub fn parse_command(cmd: &str) -> Option<leopardwm_ipc::IpcCommand> {
    use leopardwm_ipc::IpcCommand;

    match cmd.to_lowercase().as_str() {
        "focus_left" => Some(IpcCommand::FocusLeft),
        "focus_right" => Some(IpcCommand::FocusRight),
        "focus_up" => Some(IpcCommand::FocusUp),
        "focus_down" => Some(IpcCommand::FocusDown),
        "focus_next" => Some(IpcCommand::FocusNext),
        "focus_prev" => Some(IpcCommand::FocusPrev),
        "move_column_left" => Some(IpcCommand::MoveColumnLeft),
        "move_column_right" => Some(IpcCommand::MoveColumnRight),
        "focus_monitor_left" => Some(IpcCommand::FocusMonitorLeft),
        "focus_monitor_right" => Some(IpcCommand::FocusMonitorRight),
        "move_to_monitor_left" => Some(IpcCommand::MoveWindowToMonitorLeft),
        "move_to_monitor_right" => Some(IpcCommand::MoveWindowToMonitorRight),
        "cycle_width_up" | "resize_grow" => Some(IpcCommand::CycleWidthUp),
        "cycle_width_down" | "resize_shrink" => Some(IpcCommand::CycleWidthDown),
        "cycle_height_up" => Some(IpcCommand::CycleHeightUp),
        "cycle_height_down" => Some(IpcCommand::CycleHeightDown),
        "equalize_heights" => Some(IpcCommand::EqualizeColumnHeights),
        "scroll_left" => Some(IpcCommand::Scroll { delta: -100.0 }),
        "scroll_right" => Some(IpcCommand::Scroll { delta: 100.0 }),
        "refresh" => Some(IpcCommand::Refresh),
        "reload" => Some(IpcCommand::Reload),
        "panic_revert" | "panic-revert" => Some(IpcCommand::PanicRevert),
        "toggle_pause" | "toggle-pause" => Some(IpcCommand::TogglePause),
        "close_window" => Some(IpcCommand::CloseWindow),
        "toggle_floating" => Some(IpcCommand::ToggleFloating),
        "toggle_fullscreen" => Some(IpcCommand::ToggleFullscreen),
        "width_third" => Some(IpcCommand::SetColumnWidth { fraction: 0.333 }),
        "width_half" => Some(IpcCommand::SetColumnWidth { fraction: 0.5 }),
        "width_two_thirds" => Some(IpcCommand::SetColumnWidth { fraction: 0.667 }),
        "equalize_widths" => Some(IpcCommand::EqualizeColumnWidths),
        "move_window_left" => Some(IpcCommand::MoveWindowLeft),
        "move_window_right" => Some(IpcCommand::MoveWindowRight),
        "expel_to_left" => Some(IpcCommand::ExpelToLeft),
        "expel_to_right" => Some(IpcCommand::ExpelToRight),
        "move_window_up" => Some(IpcCommand::MoveWindowUp),
        "move_window_down" => Some(IpcCommand::MoveWindowDown),
        "switch_workspace_1" => Some(IpcCommand::SwitchWorkspace { index: 1 }),
        "switch_workspace_2" => Some(IpcCommand::SwitchWorkspace { index: 2 }),
        "switch_workspace_3" => Some(IpcCommand::SwitchWorkspace { index: 3 }),
        "switch_workspace_4" => Some(IpcCommand::SwitchWorkspace { index: 4 }),
        "switch_workspace_5" => Some(IpcCommand::SwitchWorkspace { index: 5 }),
        "switch_workspace_6" => Some(IpcCommand::SwitchWorkspace { index: 6 }),
        "switch_workspace_7" => Some(IpcCommand::SwitchWorkspace { index: 7 }),
        "switch_workspace_8" => Some(IpcCommand::SwitchWorkspace { index: 8 }),
        "switch_workspace_9" => Some(IpcCommand::SwitchWorkspace { index: 9 }),
        "move_to_workspace_1" => Some(IpcCommand::MoveToWorkspace { index: 1 }),
        "move_to_workspace_2" => Some(IpcCommand::MoveToWorkspace { index: 2 }),
        "move_to_workspace_3" => Some(IpcCommand::MoveToWorkspace { index: 3 }),
        "move_to_workspace_4" => Some(IpcCommand::MoveToWorkspace { index: 4 }),
        "move_to_workspace_5" => Some(IpcCommand::MoveToWorkspace { index: 5 }),
        "move_to_workspace_6" => Some(IpcCommand::MoveToWorkspace { index: 6 }),
        "move_to_workspace_7" => Some(IpcCommand::MoveToWorkspace { index: 7 }),
        "move_to_workspace_8" => Some(IpcCommand::MoveToWorkspace { index: 8 }),
        "move_to_workspace_9" => Some(IpcCommand::MoveToWorkspace { index: 9 }),
        _ => None,
    }
}

/// Deprecated hotkey command names that should be removed during migration.
const DEPRECATED_HOTKEY_COMMANDS: &[&str] = &[
    "width_third",
    "width_half",
    "width_two_thirds",
];

/// Hotkey command renames: (old_name, new_name).
const HOTKEY_COMMAND_RENAMES: &[(&str, &str)] = &[
    ("resize_grow", "cycle_width_up"),
    ("resize_shrink", "cycle_width_down"),
];

impl Config {
    /// Migrate deprecated hotkey bindings: rename old commands and remove obsolete ones.
    fn migrate_hotkey_bindings(bindings: &mut HashMap<String, String>) {
        // Rename old command names to new ones
        for (old, new) in HOTKEY_COMMAND_RENAMES {
            for value in bindings.values_mut() {
                if value == old {
                    *value = new.to_string();
                }
            }
        }
        // Remove bindings for deprecated commands
        bindings.retain(|_, cmd| {
            !DEPRECATED_HOTKEY_COMMANDS.contains(&cmd.as_str())
        });
    }

    /// Load configuration from standard locations.
    ///
    /// Tries the following locations in order:
    /// 1. `%APPDATA%/leopardwm/config.toml`
    /// 2. `~/.config/leopardwm/config.toml`
    /// 3. `./config.toml`
    ///
    /// Returns default config if no file is found.
    pub fn load() -> Result<Self> {
        let paths = config_paths();

        for path in &paths {
            if path.exists() {
                tracing::info!("Loading config from: {}", path.display());
                return Self::load_from_path(path);
            }
        }

        tracing::info!("No config file found, using defaults");
        Ok(Self::default())
    }

    /// Validate configuration values, clamping out-of-range fields and returning warnings.
    pub fn validate(&mut self) -> Vec<ConfigWarning> {
        let mut warnings = Vec::new();

        // gap must be >= 0
        if self.layout.gap < 0 {
            warnings.push(ConfigWarning {
                field: "layout.gap".to_string(),
                message: format!("Negative gap ({}) clamped to 0", self.layout.gap),
            });
            self.layout.gap = 0;
        }

        // outer gaps must be >= 0
        for (field, val) in [
            ("layout.outer_gap_left", &mut self.layout.outer_gap_left),
            ("layout.outer_gap_right", &mut self.layout.outer_gap_right),
            ("layout.outer_gap_top", &mut self.layout.outer_gap_top),
            ("layout.outer_gap_bottom", &mut self.layout.outer_gap_bottom),
        ] {
            if *val < 0 {
                warnings.push(ConfigWarning {
                    field: field.to_string(),
                    message: format!("Negative {} ({}) clamped to 0", field, *val),
                });
                *val = 0;
            }
        }

        // width_presets must not be empty
        if self.layout.width_presets.is_empty() {
            warnings.push(ConfigWarning {
                field: "layout.width_presets".to_string(),
                message: "Empty width_presets, using defaults".to_string(),
            });
            self.layout.width_presets = default_width_presets();
        }

        // height_presets must not be empty
        if self.layout.height_presets.is_empty() {
            warnings.push(ConfigWarning {
                field: "layout.height_presets".to_string(),
                message: "Empty height_presets, using defaults".to_string(),
            });
            self.layout.height_presets = default_height_presets();
        }

        // focus_follows_mouse_delay_ms must be >= 50 when enabled
        if self.behavior.focus_follows_mouse && self.behavior.focus_follows_mouse_delay_ms < 50 {
            warnings.push(ConfigWarning {
                field: "behavior.focus_follows_mouse_delay_ms".to_string(),
                message: format!(
                    "focus_follows_mouse_delay_ms ({}) below minimum 50, clamped to 50",
                    self.behavior.focus_follows_mouse_delay_ms
                ),
            });
            self.behavior.focus_follows_mouse_delay_ms = 50;
        }

        // snap_hints.duration_ms must be >= 50 when enabled
        if self.snap_hints.enabled && self.snap_hints.duration_ms < 50 {
            warnings.push(ConfigWarning {
                field: "snap_hints.duration_ms".to_string(),
                message: format!(
                    "snap_hints.duration_ms ({}) below minimum 50, clamped to 50",
                    self.snap_hints.duration_ms
                ),
            });
            self.snap_hints.duration_ms = 50;
        }

        // active_border_color must be exactly 6 hex characters
        {
            let color = &self.appearance.active_border_color;
            let is_valid = color.len() == 6 && color.chars().all(|c| c.is_ascii_hexdigit());
            if !is_valid {
                warnings.push(ConfigWarning {
                    field: "appearance.active_border_color".to_string(),
                    message: format!(
                        "Invalid hex color '{}' (must be 6 hex chars, e.g. \"4285F4\"), reset to default",
                        color
                    ),
                });
                self.appearance.active_border_color = default_active_border_color();
            }
        }

        // behavior.log_level must be one of trace/debug/info/warn/error
        {
            let normalized = self.behavior.log_level.to_lowercase();
            let valid = matches!(
                normalized.as_str(),
                "trace" | "debug" | "info" | "warn" | "error"
            );
            if !valid {
                warnings.push(ConfigWarning {
                    field: "behavior.log_level".to_string(),
                    message: format!(
                        "Invalid log_level '{}' (must be trace/debug/info/warn/error), reset to default",
                        self.behavior.log_level
                    ),
                });
                self.behavior.log_level = default_log_level();
            }
        }

        warnings
    }

    /// Compile window rules into pre-compiled regex patterns for efficient matching.
    ///
    /// Invalid regex patterns are logged as warnings and their rules are skipped.
    pub fn compile_window_rules(&self) -> Vec<CompiledWindowRule> {
        let mut compiled = Vec::new();

        for rule in &self.window_rules {
            let class_regex = match &rule.match_class {
                Some(pattern) => match regex::RegexBuilder::new(pattern)
                    .size_limit(1_000_000)
                    .build()
                {
                    Ok(re) => Some(re),
                    Err(e) => {
                        tracing::warn!(
                            "Invalid regex in window rule match_class '{}': {}. Skipping rule.",
                            pattern,
                            e
                        );
                        continue;
                    }
                },
                None => None,
            };

            let title_regex = match &rule.match_title {
                Some(pattern) => match regex::RegexBuilder::new(pattern)
                    .size_limit(1_000_000)
                    .build()
                {
                    Ok(re) => Some(re),
                    Err(e) => {
                        tracing::warn!(
                            "Invalid regex in window rule match_title '{}': {}. Skipping rule.",
                            pattern,
                            e
                        );
                        continue;
                    }
                },
                None => None,
            };

            compiled.push(CompiledWindowRule {
                class_regex,
                title_regex,
                match_executable: rule.match_executable.clone(),
                action: rule.action,
                width: rule.width,
                height: rule.height,
            });
        }

        // Append built-in ignore rules (after user rules so user can override)
        for exe in BUILTIN_IGNORE_EXECUTABLES {
            compiled.push(CompiledWindowRule {
                class_regex: None,
                title_regex: None,
                match_executable: Some(exe.to_string()),
                action: WindowAction::Ignore,
                width: None,
                height: None,
            });
        }

        compiled
    }

    /// Load configuration from a specific path.
    pub fn load_from_path(path: &PathBuf) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let mut config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        // Migrate legacy `outer_gap` → per-side outer gap fields.
        config.layout.migrate_outer_gap();

        // Migrate deprecated hotkey command names.
        Self::migrate_hotkey_bindings(&mut config.hotkeys.bindings);

        // Merge default hotkeys: any command not already bound by the user
        // gets its default binding. This ensures new hotkeys automatically
        // appear for existing users without overriding their customizations.
        let user_commands: HashSet<String> =
            config.hotkeys.bindings.values().cloned().collect();
        for (key, cmd) in HotkeyConfig::default().bindings {
            if !user_commands.contains(&cmd) {
                config.hotkeys.bindings.insert(key, cmd);
            }
        }

        Ok(config)
    }

    /// Save configuration to the primary config path.
    ///
    /// Serializes the config to TOML and writes to `config_paths()[0]`.
    /// Creates parent directories if they don't exist.
    pub fn save(&self) -> Result<()> {
        let paths = config_paths();
        let path = paths
            .first()
            .ok_or_else(|| anyhow::anyhow!("No config path available"))?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
        }

        let content = toml::to_string_pretty(self)
            .context("Failed to serialize config to TOML")?;

        fs::write(path, &content)
            .with_context(|| format!("Failed to write config file: {}", path.display()))?;

        tracing::info!("Config saved to: {}", path.display());
        Ok(())
    }
}

/// Write the commented default config to disk if no config file exists.
/// Returns Ok(Some(path)) if a file was created, Ok(None) if it already exists.
pub fn ensure_config_on_disk() -> Result<Option<PathBuf>> {
    let paths = config_paths();
    // If any config file already exists, do nothing
    for path in &paths {
        if path.exists() {
            return Ok(None);
        }
    }
    // Write to the primary path
    let path = paths
        .first()
        .ok_or_else(|| anyhow::anyhow!("No config path available"))?
        .clone();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, generate_default_config_content())?;

    // Also ensure data directory exists (for workspace persistence)
    if let Some(proj_dirs) = ProjectDirs::from("", "", "leopardwm") {
        let data_dir = proj_dirs.data_dir();
        let _ = fs::create_dir_all(data_dir);
    }

    Ok(Some(path))
}

/// Generate commented default config content for hand-editing.
///
/// Keep in sync with cli/src/main.rs:generate_default_config()
fn generate_default_config_content() -> String {
    r#"# LeopardWM Configuration
# https://github.com/jcardama/LeopardWM

[layout]
# Gap between columns in pixels
gap = 10

# Outer gaps at the edges of the viewport in pixels
outer_gap_left = 10
outer_gap_right = 10
outer_gap_top = 10
outer_gap_bottom = 10

# Width presets (fractions of usable viewport width).
# First preset is used as the default width for new columns.
width_presets = [0.333, 0.5, 0.667]

# Height presets (fractions of column height / weight).
height_presets = [0.333, 0.5, 0.667]

# Centering mode: "center" or "just_in_view"
# - center: Always center the focused column
# - just_in_view: Only scroll if focused column would be outside viewport
centering_mode = "center"

[appearance]

[behavior]
# Automatically focus new windows when they appear
focus_new_windows = true

# Track focus changes from Windows (sync with Alt-Tab, etc.)
track_focus_changes = true

# Log level: trace, debug, info, warn, error
log_level = "info"

# Focus follows mouse (hover to focus)
focus_follows_mouse = false

[hotkeys]
# Navigation (focus) — Ctrl+Alt + HJKL
"Ctrl+Alt+H" = "focus_left"
"Ctrl+Alt+L" = "focus_right"
"Ctrl+Alt+K" = "focus_up"
"Ctrl+Alt+J" = "focus_down"

# Move column — Ctrl+Alt+Shift
"Ctrl+Alt+Shift+H" = "move_column_left"
"Ctrl+Alt+Shift+L" = "move_column_right"

# Width cycling — Ctrl+Alt + Minus/Equals
"Ctrl+Alt+Minus" = "cycle_width_down"
"Ctrl+Alt+Equals" = "cycle_width_up"
"Ctrl+Alt+0" = "equalize_widths"

# Height cycling — Ctrl+Alt+Shift + Minus/Equals/0
"Ctrl+Alt+Shift+Minus" = "cycle_height_down"
"Ctrl+Alt+Shift+Equals" = "cycle_height_up"
"Ctrl+Alt+Shift+0" = "equalize_heights"

# Monitor focus — Ctrl+Alt+Win
"Ctrl+Alt+Win+Comma" = "focus_monitor_left"
"Ctrl+Alt+Win+Period" = "focus_monitor_right"

# Move to monitor — Ctrl+Alt+Win+Shift
"Ctrl+Alt+Win+Shift+Comma" = "move_to_monitor_left"
"Ctrl+Alt+Win+Shift+Period" = "move_to_monitor_right"

# Window management
"Ctrl+Alt+W" = "close_window"
"Ctrl+Alt+F" = "toggle_floating"
"Ctrl+Alt+Shift+F" = "toggle_fullscreen"
"Ctrl+Alt+P" = "toggle_pause"
"Ctrl+Alt+R" = "refresh"
"Ctrl+Alt+Shift+R" = "reload"

# Emergency restore + stop daemon
"Win+Ctrl+Escape" = "panic_revert"

[gestures]
# Touchpad gesture support
enabled = true
swipe_left = "focus_left"
swipe_right = "focus_right"
swipe_up = "focus_up"
swipe_down = "focus_down"

[snap_hints]
# Visual snap hint overlays during resize
enabled = true
duration_ms = 200
opacity = 128

# [[window_rules]]
# match_class = "Chrome_WidgetWin_1"
# match_title = ".*DevTools.*"
# action = "float"
"#
    .to_string()
}

/// Get all possible config file paths in priority order.
pub fn config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // 1. Windows standard: %APPDATA%/leopardwm/config/config.toml
    if let Some(proj_dirs) = ProjectDirs::from("", "", "leopardwm") {
        paths.push(proj_dirs.config_dir().join("config.toml"));
    }

    // 2. Unix-style: ~/.config/leopardwm/config.toml
    if let Some(home) = dirs_home() {
        paths.push(home.join(".config").join("leopardwm").join("config.toml"));
    }

    // 3. Current directory: ./config.toml
    paths.push(PathBuf::from("config.toml"));

    paths
}

/// Get the user's home directory.
fn dirs_home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.layout.gap, 10);
        assert_eq!(config.layout.outer_gap_left, 10);
        assert_eq!(config.layout.outer_gap_right, 10);
        assert_eq!(config.layout.outer_gap_top, 10);
        assert_eq!(config.layout.outer_gap_bottom, 10);
        assert_eq!(config.layout.width_presets, vec![0.333, 0.5, 0.667]);
        assert_eq!(config.layout.centering_mode, CenteringModeConfig::Center);
        assert!(config.behavior.focus_new_windows);
    }

    #[test]
    fn test_config_serialization_roundtrip() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.layout.gap, config.layout.gap);
        assert_eq!(parsed.layout.centering_mode, config.layout.centering_mode);
    }

    #[test]
    fn test_config_partial_parse() {
        // Config with only some fields should use defaults for the rest
        let toml_str = r#"
            [layout]
            gap = 20
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.layout.gap, 20);
        assert_eq!(config.layout.outer_gap_left, 10); // default
        assert_eq!(config.layout.width_presets, vec![0.333, 0.5, 0.667]); // default
    }

    #[test]
    fn test_centering_mode_conversion() {
        let config_center = CenteringModeConfig::Center;
        let config_just_in_view = CenteringModeConfig::JustInView;

        let mode_center: CenteringMode = config_center.into();
        let mode_just_in_view: CenteringMode = config_just_in_view.into();

        assert_eq!(mode_center, CenteringMode::Center);
        assert_eq!(mode_just_in_view, CenteringMode::JustInView);
    }

    #[test]
    fn test_config_paths_not_empty() {
        let paths = config_paths();
        assert!(!paths.is_empty());
    }

    #[test]
    fn test_hotkey_config_default() {
        let config = HotkeyConfig::default();
        assert_eq!(config.bindings.len(), 47);
        assert_eq!(
            config.bindings.get("Ctrl+Alt+H"),
            Some(&"focus_left".to_string())
        );
        assert_eq!(
            config.bindings.get("Ctrl+Alt+L"),
            Some(&"focus_right".to_string())
        );
        assert_eq!(
            config.bindings.get("Ctrl+Alt+Shift+H"),
            Some(&"move_column_left".to_string())
        );
        assert_eq!(
            config.bindings.get("Ctrl+Alt+Minus"),
            Some(&"cycle_width_down".to_string())
        );
        assert_eq!(
            config.bindings.get("Ctrl+Alt+Win+Comma"),
            Some(&"focus_monitor_left".to_string())
        );
        assert_eq!(
            config.bindings.get("Win+Ctrl+Escape"),
            Some(&"panic_revert".to_string())
        );
    }

    #[test]
    fn test_parse_command() {
        use leopardwm_ipc::IpcCommand;

        assert_eq!(parse_command("focus_left"), Some(IpcCommand::FocusLeft));
        assert_eq!(parse_command("FOCUS_RIGHT"), Some(IpcCommand::FocusRight));
        assert_eq!(
            parse_command("move_column_left"),
            Some(IpcCommand::MoveColumnLeft)
        );
        assert_eq!(
            parse_command("focus_monitor_left"),
            Some(IpcCommand::FocusMonitorLeft)
        );
        assert_eq!(
            parse_command("resize_grow"),
            Some(IpcCommand::CycleWidthUp)
        );
        assert_eq!(
            parse_command("resize_shrink"),
            Some(IpcCommand::CycleWidthDown)
        );
        assert_eq!(
            parse_command("cycle_width_up"),
            Some(IpcCommand::CycleWidthUp)
        );
        assert_eq!(
            parse_command("cycle_height_up"),
            Some(IpcCommand::CycleHeightUp)
        );
        assert_eq!(
            parse_command("equalize_heights"),
            Some(IpcCommand::EqualizeColumnHeights)
        );
        assert_eq!(parse_command("refresh"), Some(IpcCommand::Refresh));
        assert_eq!(parse_command("panic_revert"), Some(IpcCommand::PanicRevert));
        assert_eq!(parse_command("PANIC-REVERT"), Some(IpcCommand::PanicRevert));
        assert_eq!(parse_command("toggle_pause"), Some(IpcCommand::TogglePause));
        assert_eq!(
            parse_command("move_window_left"),
            Some(IpcCommand::MoveWindowLeft)
        );
        assert_eq!(
            parse_command("move_window_right"),
            Some(IpcCommand::MoveWindowRight)
        );
        assert_eq!(
            parse_command("expel_to_left"),
            Some(IpcCommand::ExpelToLeft)
        );
        assert_eq!(
            parse_command("expel_to_right"),
            Some(IpcCommand::ExpelToRight)
        );
        assert_eq!(
            parse_command("move_window_up"),
            Some(IpcCommand::MoveWindowUp)
        );
        assert_eq!(
            parse_command("move_window_down"),
            Some(IpcCommand::MoveWindowDown)
        );
        assert_eq!(parse_command("unknown_command"), None);
    }

    #[test]
    fn test_hotkey_config_serialization() {
        let toml_str = r#"
            [hotkeys]
            "Win+A" = "focus_left"
            "Ctrl+Alt+B" = "focus_right"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.hotkeys.bindings.get("Win+A"),
            Some(&"focus_left".to_string())
        );
        assert_eq!(
            config.hotkeys.bindings.get("Ctrl+Alt+B"),
            Some(&"focus_right".to_string())
        );
    }

    #[test]
    fn test_hotkey_merge_adds_missing_defaults() {
        // Simulate a user config with only one hotkey — load_from_path
        // would merge defaults for unbound commands.
        let defaults = HotkeyConfig::default();
        let mut user = HotkeyConfig {
            scroll_modifier: default_scroll_modifier(),
            bindings: HashMap::new(),
        };
        // User only binds focus_left to a custom key
        user.bindings
            .insert("Ctrl+Alt+X".to_string(), "focus_left".to_string());

        // Merge: commands not bound by user get default binding
        let user_commands: HashSet<String> = user.bindings.values().cloned().collect();
        for (key, cmd) in defaults.bindings.iter() {
            if !user_commands.contains(cmd) {
                user.bindings.insert(key.clone(), cmd.clone());
            }
        }

        // User's custom binding preserved (default focus_left key not added)
        assert_eq!(
            user.bindings.get("Ctrl+Alt+X"),
            Some(&"focus_left".to_string())
        );
        assert!(!user.bindings.contains_key("Ctrl+Alt+H"));

        // New commands from defaults are present
        assert_eq!(
            user.bindings.get("Ctrl+Alt+Bracket_Left"),
            Some(&"move_window_left".to_string())
        );
        assert_eq!(
            user.bindings.get("Ctrl+Alt+Shift+J"),
            Some(&"move_window_down".to_string())
        );
    }

    #[test]
    fn test_width_presets_defaults() {
        let config = Config::default();
        assert_eq!(config.layout.width_presets, vec![0.333, 0.5, 0.667]);
        assert_eq!(config.layout.height_presets, vec![0.333, 0.5, 0.667]);
    }

    #[test]
    fn test_default_column_width_px() {
        let config = LayoutConfig::default();
        // base = 1920 - 10 - 10 + 10 = 1910
        // width = 0.333 * 1910 - 10 = 626
        let width = config.default_column_width_px(1920);
        let base = 1920 - config.outer_gap_left - config.outer_gap_right + config.gap;
        assert_eq!(width, (base as f64 * 0.333 - config.gap as f64).round() as i32);
    }

    #[test]
    fn test_window_rule_matches_class() {
        let rule = WindowRule {
            match_class: Some("Notepad".to_string()),
            match_title: None,
            match_executable: None,
            action: WindowAction::Float,
            width: None,
            height: None,
        };

        assert!(rule.matches("Notepad", "Untitled - Notepad", "notepad.exe"));
        assert!(!rule.matches("Chrome_WidgetWin_1", "Google Chrome", "chrome.exe"));
    }

    #[test]
    fn test_window_rule_matches_title_regex() {
        let rule = WindowRule {
            match_class: None,
            match_title: Some(".*DevTools.*".to_string()),
            match_executable: None,
            action: WindowAction::Float,
            width: Some(800),
            height: Some(600),
        };

        assert!(rule.matches(
            "Chrome_WidgetWin_1",
            "DevTools - localhost:3000",
            "chrome.exe"
        ));
        assert!(rule.matches("SomeClass", "Firefox DevTools", "firefox.exe"));
        assert!(!rule.matches("Chrome_WidgetWin_1", "Google Chrome", "chrome.exe"));
    }

    #[test]
    fn test_window_rule_matches_executable() {
        let rule = WindowRule {
            match_class: None,
            match_title: None,
            match_executable: Some("spotify.exe".to_string()),
            action: WindowAction::Float,
            width: None,
            height: None,
        };

        assert!(rule.matches("SpotifyClass", "Spotify - Song Title", "spotify.exe"));
        assert!(rule.matches("SpotifyClass", "Spotify - Song Title", "SPOTIFY.EXE")); // Case insensitive
        assert!(!rule.matches("SpotifyClass", "Spotify - Song Title", "chrome.exe"));
    }

    #[test]
    fn test_window_rule_matches_combined() {
        let rule = WindowRule {
            match_class: Some("Chrome.*".to_string()),
            match_title: Some(".*YouTube.*".to_string()),
            match_executable: None,
            action: WindowAction::Tile,
            width: None,
            height: None,
        };

        // Both patterns must match
        assert!(rule.matches(
            "Chrome_WidgetWin_1",
            "YouTube - Google Chrome",
            "chrome.exe"
        ));
        assert!(!rule.matches("Firefox", "YouTube - Mozilla Firefox", "firefox.exe")); // Class doesn't match
        assert!(!rule.matches("Chrome_WidgetWin_1", "Google Chrome", "chrome.exe"));
        // Title doesn't match
    }

    #[test]
    fn test_window_rule_no_criteria_matches_nothing() {
        let rule = WindowRule {
            match_class: None,
            match_title: None,
            match_executable: None,
            action: WindowAction::Ignore,
            width: None,
            height: None,
        };

        assert!(!rule.matches("AnyClass", "Any Title", "any.exe"));
    }

    #[test]
    fn test_window_rule_config_parse() {
        let toml_str = r#"
            [[window_rules]]
            match_class = "Notepad"
            action = "float"
            width = 800
            height = 600

            [[window_rules]]
            match_executable = "spotify.exe"
            action = "float"

            [[window_rules]]
            match_title = ".*dialog.*"
            action = "ignore"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.window_rules.len(), 3);

        assert_eq!(
            config.window_rules[0].match_class,
            Some("Notepad".to_string())
        );
        assert_eq!(config.window_rules[0].action, WindowAction::Float);
        assert_eq!(config.window_rules[0].width, Some(800));
        assert_eq!(config.window_rules[0].height, Some(600));

        assert_eq!(
            config.window_rules[1].match_executable,
            Some("spotify.exe".to_string())
        );
        assert_eq!(config.window_rules[1].action, WindowAction::Float);

        assert_eq!(
            config.window_rules[2].match_title,
            Some(".*dialog.*".to_string())
        );
        assert_eq!(config.window_rules[2].action, WindowAction::Ignore);
    }

    #[test]
    fn test_window_action_default() {
        let action = WindowAction::default();
        assert_eq!(action, WindowAction::Tile);
    }

    #[test]
    fn test_snap_hint_config_default() {
        let config = SnapHintConfig::default();
        assert!(config.enabled);
        assert_eq!(config.duration_ms, 200);
        assert_eq!(config.opacity, 128);
    }

    #[test]
    fn test_snap_hint_config_serialization() {
        let toml_str = r#"
            [snap_hints]
            enabled = true
            duration_ms = 300
            opacity = 200
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.snap_hints.enabled);
        assert_eq!(config.snap_hints.duration_ms, 300);
        assert_eq!(config.snap_hints.opacity, 200);
    }

    #[test]
    fn test_focus_follows_mouse_default() {
        let config = Config::default();
        assert!(!config.behavior.focus_follows_mouse);
        assert_eq!(config.behavior.focus_follows_mouse_delay_ms, 100);
    }

    #[test]
    fn test_focus_follows_mouse_serialization() {
        let toml_str = r#"
            [behavior]
            focus_follows_mouse = true
            focus_follows_mouse_delay_ms = 200
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.behavior.focus_follows_mouse);
        assert_eq!(config.behavior.focus_follows_mouse_delay_ms, 200);
    }

    // =========================================================================
    // Window Rule Edge Cases
    // =========================================================================

    #[test]
    fn test_window_rule_multiple_matches_uses_first() {
        // When multiple rules could match, the first one wins
        let rules = vec![
            WindowRule {
                match_class: Some("Notepad".to_string()),
                match_title: None,
                match_executable: None,
                action: WindowAction::Float,
                width: Some(800),
                height: Some(600),
            },
            WindowRule {
                match_class: Some("Notepad".to_string()),
                match_title: None,
                match_executable: None,
                action: WindowAction::Ignore, // Different action
                width: None,
                height: None,
            },
        ];

        // First matching rule should be returned
        let mut matched_action = WindowAction::Tile; // Default
        for rule in &rules {
            if rule.matches("Notepad", "Untitled", "notepad.exe") {
                matched_action = rule.action;
                break;
            }
        }
        assert_eq!(matched_action, WindowAction::Float);
    }

    #[test]
    fn test_window_rule_regex_special_chars() {
        // Test regex with special characters that need escaping
        let rule = WindowRule {
            match_class: None,
            match_title: Some(r"^\[DEBUG\].*$".to_string()), // Escaped brackets
            match_executable: None,
            action: WindowAction::Ignore,
            width: None,
            height: None,
        };

        assert!(rule.matches("AnyClass", "[DEBUG] Application started", "app.exe"));
        assert!(!rule.matches("AnyClass", "DEBUG Application started", "app.exe"));
    }

    #[test]
    fn test_window_rule_regex_case_sensitivity() {
        // By default, regex is case-sensitive
        let rule = WindowRule {
            match_class: None,
            match_title: Some("Error".to_string()),
            match_executable: None,
            action: WindowAction::Float,
            width: None,
            height: None,
        };

        assert!(rule.matches("AnyClass", "Error Dialog", "app.exe"));
        assert!(!rule.matches("AnyClass", "error dialog", "app.exe")); // Case mismatch
    }

    #[test]
    fn test_window_rule_regex_case_insensitive() {
        // Test case-insensitive regex with (?i) flag
        let rule = WindowRule {
            match_class: None,
            match_title: Some("(?i)error".to_string()),
            match_executable: None,
            action: WindowAction::Float,
            width: None,
            height: None,
        };

        assert!(rule.matches("AnyClass", "Error Dialog", "app.exe"));
        assert!(rule.matches("AnyClass", "error dialog", "app.exe"));
        assert!(rule.matches("AnyClass", "ERROR DIALOG", "app.exe"));
    }

    #[test]
    fn test_window_rule_partial_config_class_only() {
        // Rule with only class specified
        let rule = WindowRule {
            match_class: Some("MyClass".to_string()),
            match_title: None,
            match_executable: None,
            action: WindowAction::Tile,
            width: None,
            height: None,
        };

        assert!(rule.matches("MyClass", "Any Title", "any.exe"));
        assert!(rule.matches("MyClass", "Different Title", "different.exe"));
        assert!(!rule.matches("OtherClass", "Any Title", "any.exe"));
    }

    #[test]
    fn test_window_rule_partial_config_title_only() {
        // Rule with only title specified
        let rule = WindowRule {
            match_class: None,
            match_title: Some(".*Settings.*".to_string()),
            match_executable: None,
            action: WindowAction::Float,
            width: None,
            height: None,
        };

        assert!(rule.matches("AnyClass", "App Settings", "any.exe"));
        assert!(rule.matches("DifferentClass", "Settings Panel", "different.exe"));
        assert!(!rule.matches("AnyClass", "Main Window", "any.exe"));
    }

    #[test]
    fn test_window_rule_partial_config_executable_only() {
        // Rule with only executable specified
        let rule = WindowRule {
            match_class: None,
            match_title: None,
            match_executable: Some("notepad.exe".to_string()),
            action: WindowAction::Tile,
            width: None,
            height: None,
        };

        assert!(rule.matches("AnyClass", "Any Title", "notepad.exe"));
        assert!(rule.matches("AnyClass", "Any Title", "NOTEPAD.EXE")); // Case insensitive
        assert!(!rule.matches("AnyClass", "Any Title", "wordpad.exe"));
    }

    #[test]
    fn test_window_rule_invalid_regex_returns_false() {
        // Invalid regex should not match anything
        let rule = WindowRule {
            match_class: None,
            match_title: Some("[invalid(regex".to_string()), // Invalid regex
            match_executable: None,
            action: WindowAction::Float,
            width: None,
            height: None,
        };

        // Should return false because regex is invalid
        assert!(!rule.matches("AnyClass", "Any Title", "any.exe"));
    }

    #[test]
    fn test_window_rule_empty_strings_match() {
        // Test matching against empty strings
        let rule = WindowRule {
            match_class: Some(".*".to_string()), // Match anything including empty
            match_title: None,
            match_executable: None,
            action: WindowAction::Float,
            width: None,
            height: None,
        };

        assert!(rule.matches("", "Title", "app.exe")); // Empty class matches .*
        assert!(rule.matches("SomeClass", "Title", "app.exe"));
    }

    #[test]
    fn test_window_rule_width_height_optional() {
        // Width and height are optional and independent
        let toml_str = r#"
            [[window_rules]]
            match_class = "Test"
            action = "float"
            width = 1000
            # height not specified

            [[window_rules]]
            match_class = "Test2"
            action = "float"
            # width not specified
            height = 800
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();

        assert_eq!(config.window_rules[0].width, Some(1000));
        assert_eq!(config.window_rules[0].height, None);

        assert_eq!(config.window_rules[1].width, None);
        assert_eq!(config.window_rules[1].height, Some(800));
    }

    // =========================================================================
    // Config Validation Tests
    // =========================================================================

    #[test]
    fn test_validate_negative_gap_clamped() {
        let mut config = Config::default();
        config.layout.gap = -5;
        let warnings = config.validate();
        assert_eq!(config.layout.gap, 0);
        assert!(warnings.iter().any(|w| w.field == "layout.gap"));
    }

    #[test]
    fn test_validate_negative_outer_gap_clamped() {
        let mut config = Config::default();
        config.layout.outer_gap_left = -10;
        config.layout.outer_gap_top = -5;
        let warnings = config.validate();
        assert_eq!(config.layout.outer_gap_left, 0);
        assert_eq!(config.layout.outer_gap_top, 0);
        assert!(warnings
            .iter()
            .any(|w| w.field == "layout.outer_gap_left"));
    }

    #[test]
    fn test_validate_empty_width_presets_resets() {
        let mut config = Config::default();
        config.layout.width_presets = vec![];
        let warnings = config.validate();
        assert_eq!(config.layout.width_presets, vec![0.333, 0.5, 0.667]);
        assert!(warnings
            .iter()
            .any(|w| w.field == "layout.width_presets"));
    }

    #[test]
    fn test_validate_focus_delay_below_min_clamped() {
        let mut config = Config::default();
        config.behavior.focus_follows_mouse = true;
        config.behavior.focus_follows_mouse_delay_ms = 10;
        let warnings = config.validate();
        assert_eq!(config.behavior.focus_follows_mouse_delay_ms, 50);
        assert!(warnings
            .iter()
            .any(|w| w.field == "behavior.focus_follows_mouse_delay_ms"));
    }

    #[test]
    fn test_validate_snap_duration_below_min_clamped() {
        let mut config = Config::default();
        config.snap_hints.enabled = true;
        config.snap_hints.duration_ms = 20;
        let warnings = config.validate();
        assert_eq!(config.snap_hints.duration_ms, 50);
        assert!(warnings.iter().any(|w| w.field == "snap_hints.duration_ms"));
    }

    #[test]
    fn test_validate_invalid_log_level_resets_to_default() {
        let mut config = Config::default();
        config.behavior.log_level = "verbose".to_string();
        let warnings = config.validate();
        assert_eq!(config.behavior.log_level, "info");
        assert!(warnings.iter().any(|w| w.field == "behavior.log_level"));
    }

    #[test]
    fn test_validate_log_level_case_insensitive_valid() {
        let mut config = Config::default();
        config.behavior.log_level = "DEBUG".to_string();
        let warnings = config.validate();
        assert!(warnings.iter().all(|w| w.field != "behavior.log_level"));
        assert_eq!(config.behavior.log_level, "DEBUG");
    }

    #[test]
    fn test_validate_valid_config_no_warnings() {
        let mut config = Config::default();
        let warnings = config.validate();
        assert!(
            warnings.is_empty(),
            "Default config should produce no warnings, got: {:?}",
            warnings
        );
    }

    // =========================================================================
    // Compiled Window Rule Tests
    // =========================================================================

    #[test]
    fn test_compiled_window_rule_matches() {
        let config = Config {
            window_rules: vec![
                WindowRule {
                    match_class: Some("Chrome.*".to_string()),
                    match_title: Some(".*YouTube.*".to_string()),
                    match_executable: None,
                    action: WindowAction::Float,
                    width: Some(1024),
                    height: Some(768),
                },
                WindowRule {
                    match_class: None,
                    match_title: None,
                    match_executable: Some("notepad.exe".to_string()),
                    action: WindowAction::Tile,
                    width: None,
                    height: None,
                },
            ],
            ..Default::default()
        };

        let compiled = config.compile_window_rules();
        assert_eq!(
            compiled.len(),
            2 + BUILTIN_IGNORE_EXECUTABLES.len()
        );

        // First rule: class + title regex
        assert!(compiled[0].matches(
            "Chrome_WidgetWin_1",
            "YouTube - Google Chrome",
            "chrome.exe"
        ));
        assert!(!compiled[0].matches("Firefox", "YouTube", "firefox.exe")); // class doesn't match
        assert!(!compiled[0].matches("Chrome_WidgetWin_1", "Google Chrome", "chrome.exe")); // title doesn't match

        // Second rule: executable only
        assert!(compiled[1].matches("AnyClass", "Any Title", "notepad.exe"));
        assert!(compiled[1].matches("AnyClass", "Any Title", "NOTEPAD.EXE")); // case insensitive
        assert!(!compiled[1].matches("AnyClass", "Any Title", "wordpad.exe"));
    }

    #[test]
    fn test_compiled_window_rule_invalid_regex_skipped() {
        let config = Config {
            window_rules: vec![
                WindowRule {
                    match_class: Some("[invalid(regex".to_string()), // Invalid regex
                    match_title: None,
                    match_executable: None,
                    action: WindowAction::Float,
                    width: None,
                    height: None,
                },
                WindowRule {
                    match_class: Some("ValidClass".to_string()),
                    match_title: None,
                    match_executable: None,
                    action: WindowAction::Tile,
                    width: None,
                    height: None,
                },
            ],
            ..Default::default()
        };

        let compiled = config.compile_window_rules();
        // First rule should be skipped due to invalid regex
        assert_eq!(
            compiled.len(),
            1 + BUILTIN_IGNORE_EXECUTABLES.len()
        );
        assert!(compiled[0].matches("ValidClass", "Any Title", "any.exe"));
    }

    #[test]
    fn test_focus_new_windows_false_parsed() {
        let toml_str = r#"
            [behavior]
            focus_new_windows = false
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.behavior.focus_new_windows);
    }

    #[test]
    fn test_focus_new_windows_defaults_to_true() {
        let toml_str = r#"
            [behavior]
            log_level = "info"
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.behavior.focus_new_windows);
    }

    // =========================================================================
    // Hex Color Validation Tests (Iteration 34)
    // =========================================================================

    #[test]
    fn test_validate_hex_color_valid() {
        let mut config = Config::default();
        config.appearance.active_border_color = "ff0000".to_string();
        let warnings = config.validate();
        assert_eq!(config.appearance.active_border_color, "ff0000");
        assert!(!warnings
            .iter()
            .any(|w| w.field == "appearance.active_border_color"));
    }

    #[test]
    fn test_validate_hex_color_invalid_chars() {
        let mut config = Config::default();
        config.appearance.active_border_color = "ZZZZZZ".to_string();
        let warnings = config.validate();
        assert_eq!(
            config.appearance.active_border_color,
            default_active_border_color()
        );
        assert!(warnings
            .iter()
            .any(|w| w.field == "appearance.active_border_color"));
    }

    #[test]
    fn test_validate_hex_color_too_short() {
        let mut config = Config::default();
        config.appearance.active_border_color = "FFF".to_string();
        let warnings = config.validate();
        assert_eq!(
            config.appearance.active_border_color,
            default_active_border_color()
        );
        assert!(warnings
            .iter()
            .any(|w| w.field == "appearance.active_border_color"));
    }

    #[test]
    fn test_validate_hex_color_with_hash_prefix() {
        let mut config = Config::default();
        config.appearance.active_border_color = "#4285F4".to_string();
        let warnings = config.validate();
        // Hash prefix makes it 7 chars, so it should be rejected
        assert_eq!(
            config.appearance.active_border_color,
            default_active_border_color()
        );
        assert!(warnings
            .iter()
            .any(|w| w.field == "appearance.active_border_color"));
    }

    // =========================================================================
    // Config Edge Case Tests (Iteration 34)
    // =========================================================================

    #[test]
    fn test_empty_config_uses_defaults() {
        let config: Config = toml::from_str("").unwrap();
        let default = Config::default();
        assert_eq!(config.layout.gap, default.layout.gap);
        assert_eq!(config.layout.outer_gap_left, default.layout.outer_gap_left);
        assert_eq!(
            config.layout.width_presets,
            default.layout.width_presets
        );
        assert_eq!(
            config.appearance.active_border_color,
            default.appearance.active_border_color
        );
        assert!(config.behavior.focus_new_windows);
        assert!(config.window_rules.is_empty());
    }

    #[test]
    fn test_all_zero_numeric_values() {
        let toml_str = r#"
            [layout]
            gap = 0
            outer_gap_left = 0
            outer_gap_right = 0
            outer_gap_top = 0
            outer_gap_bottom = 0
        "#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        let warnings = config.validate();
        // gap=0 and outer gaps=0 are valid (not negative)
        assert_eq!(config.layout.gap, 0);
        assert_eq!(config.layout.outer_gap_left, 0);
        assert!(!warnings.iter().any(|w| w.field == "layout.gap"));
    }

    #[test]
    fn test_unknown_toml_keys_ignored() {
        let toml_str = r#"
            [layout]
            gap = 15
            unknown_key = "hello"
            another_unknown = 42
        "#;
        // serde(default) + deny_unknown_fields is NOT set, so this should parse
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.layout.gap, 15);
    }

    #[test]
    fn test_empty_hotkey_bindings() {
        let toml_str = r#"
            [hotkeys]
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.hotkeys.bindings.is_empty());
    }

    #[test]
    fn test_hex_color_case_insensitive() {
        let mut config1 = Config::default();
        config1.appearance.active_border_color = "ff0000".to_string();
        let warnings1 = config1.validate();
        assert!(!warnings1
            .iter()
            .any(|w| w.field == "appearance.active_border_color"));

        let mut config2 = Config::default();
        config2.appearance.active_border_color = "FF0000".to_string();
        let warnings2 = config2.validate();
        assert!(!warnings2
            .iter()
            .any(|w| w.field == "appearance.active_border_color"));
    }

    // =========================================================================
    // Regex Size Limit Test (Iteration 34)
    // =========================================================================

    #[test]
    fn test_regex_size_limit_rejects_oversized_pattern() {
        // Directly verify that RegexBuilder with size_limit rejects patterns that
        // exceed the compiled NFA size limit. Use a very small limit to guarantee rejection.
        let pattern = "[a-z]{100}";
        let result = regex::RegexBuilder::new(pattern)
            .size_limit(100) // Tiny limit to guarantee rejection
            .build();
        assert!(
            result.is_err(),
            "Pattern should be rejected with a very small size limit"
        );

        // Also verify that the same pattern succeeds without a tight limit (our production limit)
        let result = regex::RegexBuilder::new(pattern)
            .size_limit(1_000_000)
            .build();
        assert!(result.is_ok(), "Pattern should succeed with 1MB limit");
    }

    // =========================================================================
    // Config Error-Path Tests (Iteration 37)
    // =========================================================================

    #[test]
    fn test_invalid_toml_syntax_returns_error() {
        let bad_toml = r#"
            [layout
            gap = 10
        "#;
        let result: Result<Config, _> = toml::from_str(bad_toml);
        assert!(
            result.is_err(),
            "Invalid TOML (missing bracket) should fail to parse"
        );
    }

    #[test]
    fn test_empty_string_parses_to_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.layout.gap, default_gap());
        assert_eq!(config.layout.outer_gap_left, default_outer_gap());
        assert_eq!(config.layout.width_presets, default_width_presets());
    }

    #[test]
    fn test_unknown_keys_are_ignored() {
        // serde(default) without deny_unknown_fields means extra keys are silently ignored
        let toml_str = r#"
            totally_unknown_section = "hello"
            [layout]
            gap = 20
            nonexistent_field = true
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.layout.gap, 20);
    }

    #[test]
    fn test_wrong_type_returns_error() {
        let toml_str = r#"
            [layout]
            gap = "not_a_number"
        "#;
        let result: Result<Config, _> = toml::from_str(toml_str);
        assert!(
            result.is_err(),
            "String where integer expected should fail to parse"
        );
    }

    #[test]
    fn test_config_save_roundtrip() {
        let dir = std::env::temp_dir().join("leopardwm_test_save_roundtrip");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let mut config = Config::default();
        config.layout.gap = 42;
        config.appearance.active_border_color = "FF0000".to_string();

        let content = toml::to_string_pretty(&config).unwrap();
        fs::write(&path, &content).unwrap();

        let loaded = Config::load_from_path(&path).unwrap();
        assert_eq!(loaded.layout.gap, 42);
        assert_eq!(loaded.appearance.active_border_color, "FF0000");

        let _ = fs::remove_dir_all(&dir);
    }
}
