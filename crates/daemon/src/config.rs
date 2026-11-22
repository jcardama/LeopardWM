//! Config lookup uses the first existing path from [`config_paths`], or defaults
//! when none exists. Loading also migrates legacy fields and merges hotkeys.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use leopardwm_core_layout::CenteringMode;
use leopardwm_ipc::log_text;
use leopardwm_platform_win32::parse_hotkey_string;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[path = "config_recovery.rs"]
mod recovery;

/// Executables whose windows should never be tiled (system dialogs, security prompts, etc.).
/// These are appended as built-in Ignore rules after user-defined rules.
const BUILTIN_IGNORE_EXECUTABLES: &[&str] = &[
    "smartscreen.exe",        // Windows Defender SmartScreen
    "consent.exe",            // UAC elevation prompt
    "msiexec.exe",            // Windows Installer
    "CredentialUIBroker.exe", // Windows credential/login prompt
    "SnippingTool.exe",       // Screen capture overlay — breaks when repositioned
];

const BUILTIN_FLOAT_RULES: &[(&str, &str)] = &[
    ("Audacity.exe", "^Importing"), // Audacity import progress popup
];

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
    /// Animation timing configuration.
    #[serde(default)]
    pub animation: AnimationConfig,
    /// Per-workspace display names.
    #[serde(default)]
    pub workspaces: WorkspacesConfig,
    /// Overview-mode configuration.
    #[serde(default)]
    pub overview: OverviewConfig,
    pub glazewm_compat: GlazewmCompatConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlazewmCompatConfig {
    pub enabled: bool,
    pub port: u16,
}

impl Default for GlazewmCompatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 6123,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OverviewConfig {
    /// How overview cards render their body.
    pub render: OverviewRender,
}

/// How overview cards render their body: live DWM thumbnails of the
/// windows, cached capture-on-hide snapshots, or the static icon
/// placeholder.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverviewRender {
    /// Live DWM thumbnail previews (last frame for windows on hidden
    /// workspaces).
    #[default]
    Live,
    /// Static placeholder bodies (app icon only).
    Placeholder,
    /// `PrintWindow` snapshots captured right before windows leave the
    /// screen (workspace switch) and on overview open; icon placeholder
    /// when no snapshot is cached yet.
    Snapshot,
}

/// Workspace configuration.
///
/// `names` is position-based: index 0 names workspace 1, index 1 names
/// workspace 2, and so on (up to 9). An empty string leaves that
/// workspace unnamed (shown by its number). Trailing entries may be
/// omitted; only workspaces you want to name need an entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspacesConfig {
    /// Display names for workspaces 1-9, by position.
    pub names: Vec<String>,
}

impl WorkspacesConfig {
    /// Resolve the display name for a 0-based workspace index, or `None`
    /// if unnamed (no entry or empty string).
    pub fn name_for(&self, index_0based: usize) -> Option<String> {
        self.names
            .get(index_0based)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }
}

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

    #[serde(flatten)]
    pub center_columns: CenterColumnsSetting,

    /// Whether center-column can scroll past content edges.
    /// When true, the first/last column will be truly centered with empty space.
    /// When false (default), scroll is clamped to content boundaries.
    #[serde(default = "default_false")]
    pub center_past_edges: bool,

    #[serde(default)]
    pub mouse_resize: MouseResize,

    #[serde(default)]
    pub offscreen_mode: OffscreenMode,

    /// Width presets for cycling (fractions of usable viewport width).
    #[serde(default = "default_width_presets")]
    pub width_presets: Vec<f64>,

    /// Which width preset new columns open at, as a 1-based index into
    /// `width_presets`. Defaults to 1 (the first preset). Out-of-range values
    /// fall back to the first preset.
    #[serde(default = "default_width_preset")]
    pub default_width_preset: usize,

    /// Height presets for cycling (fractions of column height / weight).
    #[serde(default = "default_height_presets")]
    pub height_presets: Vec<f64>,

    /// Multi-row layout configuration.
    #[serde(default)]
    pub rows: RowsConfig,

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

/// Configuration for row splits in specific workspace(s).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceRowsConfig {
    /// Target workspaces (1-based, 1..=9).
    /// Accepts a single number (`workspace = 1`), an array (`workspaces = [1, 2]`),
    /// or a comma-separated string (`workspaces = "1, 2"`).
    #[serde(
        default,
        alias = "workspace",
        deserialize_with = "deserialize_one_or_many_workspaces"
    )]
    pub workspaces: Vec<u8>,

    /// Row split as percentages, fractions, or weights, top to bottom (max 4 rows).
    #[serde(default = "default_row_split", deserialize_with = "deserialize_split")]
    pub split: Vec<String>,
}

impl WorkspaceRowsConfig {
    /// Parse row shares for this workspace configuration.
    pub fn parse_shares(&self) -> Vec<f64> {
        parse_split_shares(&self.split)
    }

    /// Check if this config targets a 1-based workspace number.
    pub fn matches_workspace(&self, ws_1based: u8) -> bool {
        self.workspaces.contains(&ws_1based)
    }
}

/// Multi-row layout configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RowsConfig {
    /// Vertical gap between rows in pixels. Default: 8.
    pub gap: i32,

    /// Per-workspace row configurations.
    /// Can specify 1 or more workspaces and the split for those workspaces.
    #[serde(default)]
    pub workspaces: Vec<WorkspaceRowsConfig>,

    /// Global/fallback row split as percentages, fractions, or weights, top to bottom.
    /// Default: ["100%"] (single row).
    #[serde(default = "default_row_split", deserialize_with = "deserialize_split")]
    pub split: Vec<String>,
}

fn default_row_gap() -> i32 {
    8
}

fn default_row_split() -> Vec<String> {
    vec!["100%".to_string()]
}

impl Default for RowsConfig {
    fn default() -> Self {
        Self {
            gap: default_row_gap(),
            workspaces: Vec::new(),
            split: default_row_split(),
        }
    }
}

impl RowsConfig {
    /// Parse row shares for a specific 0-based workspace index.
    ///
    /// By default, any unconfigured workspace has only 1 row (`vec![1.0]`).
    /// If per-workspace row configs are specified, the first matching entry is used.
    /// If no per-workspace configs are defined and a legacy/global `split` was configured,
    /// falls back to parsing `self.split`.
    pub fn parse_shares_for_workspace(&self, ws_idx_0based: usize) -> Vec<f64> {
        let ws_1based = (ws_idx_0based + 1) as u8;
        for entry in &self.workspaces {
            if entry.matches_workspace(ws_1based) {
                return entry.parse_shares();
            }
        }

        // Fall back to global split (which defaults to ["100%"] -> vec![1.0]).
        self.parse_shares()
    }

    /// Parse row shares from the global `split` field.
    pub fn parse_shares(&self) -> Vec<f64> {
        parse_split_shares(&self.split)
    }
}

/// Parse row shares from a list of split strings (percentages, fractions, or weights).
///
/// Validates that:
/// - Each entry parses as a percentage, fraction, or number > 0.
/// - At most 4 rows are accepted (entries beyond the 4th are ignored with a warning).
/// - Shares are normalized to sum to 1.0 (with a warning if raw percentages don't sum to ~100%).
/// - If no valid entries are found, falls back to `vec![1.0]` (single row).
pub fn parse_split_shares(split: &[String]) -> Vec<f64> {
    let mut raw_values = Vec::new();
    for entry in split.iter().take(4) {
        let trimmed = entry.trim().trim_end_matches('%').trim();
        if trimmed.contains('/') {
            let parts: Vec<&str> = trimmed.split('/').collect();
            if parts.len() == 2 {
                if let (Ok(num), Ok(den)) = (
                    parts[0].trim().parse::<f64>(),
                    parts[1].trim().parse::<f64>(),
                ) {
                    if den > 0.0 && num > 0.0 && (num / den).is_finite() {
                        raw_values.push(num / den);
                        continue;
                    }
                }
            }
        }
        if let Ok(val) = trimmed.parse::<f64>() {
            if val > 0.0 && val.is_finite() {
                raw_values.push(val);
            } else {
                tracing::warn!(
                    "Row split entry '{}' must be greater than 0; ignoring",
                    log_text::text(entry)
                );
            }
        } else {
            tracing::warn!(
                "Failed to parse row split entry '{}'; ignoring",
                log_text::text(entry)
            );
        }
    }

    if split.len() > 4 {
        tracing::warn!(
            "Configured {} rows, but at most 4 rows are supported; truncating to 4",
            split.len()
        );
    }

    if raw_values.is_empty() {
        tracing::warn!("No valid row split entries; falling back to 100% (single row)");
        return vec![1.0];
    }

    let sum: f64 = raw_values.iter().sum();
    if sum <= 0.0 || !sum.is_finite() {
        return vec![1.0];
    }

    if (sum - 100.0).abs() > 0.5 && (sum - 1.0).abs() > 0.01 {
        tracing::warn!(
            "Row split entries sum to {:.1} (expected 100%); normalizing shares",
            sum
        );
    }

    raw_values.iter().map(|&v| v / sum).collect()
}

fn deserialize_one_or_many_workspaces<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Helper {
        SingleNum(u8),
        NumList(Vec<u8>),
        SingleStr(String),
        StrList(Vec<String>),
    }

    let helper = Helper::deserialize(deserializer)?;
    let mut result = Vec::new();

    match helper {
        Helper::SingleNum(n) => {
            if (1..=9).contains(&n) {
                result.push(n);
            } else {
                tracing::warn!("Workspace index {} out of range 1..=9; ignoring", n);
            }
        }
        Helper::NumList(list) => {
            for n in list {
                if (1..=9).contains(&n) {
                    if !result.contains(&n) {
                        result.push(n);
                    }
                } else {
                    tracing::warn!("Workspace index {} out of range 1..=9; ignoring", n);
                }
            }
        }
        Helper::SingleStr(s) => {
            parse_workspaces_str(&s, &mut result);
        }
        Helper::StrList(list) => {
            for s in list {
                parse_workspaces_str(&s, &mut result);
            }
        }
    }

    Ok(result)
}

fn parse_workspaces_str(s: &str, out: &mut Vec<u8>) {
    for part in s.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((start_s, end_s)) = trimmed.split_once('-') {
            if let (Ok(start), Ok(end)) = (start_s.trim().parse::<u8>(), end_s.trim().parse::<u8>())
            {
                for n in start..=end {
                    if (1..=9).contains(&n) {
                        if !out.contains(&n) {
                            out.push(n);
                        }
                    } else {
                        tracing::warn!("Workspace index {} out of range 1..=9; ignoring", n);
                    }
                }
                continue;
            }
        }
        if let Ok(n) = trimmed.parse::<u8>() {
            if (1..=9).contains(&n) {
                if !out.contains(&n) {
                    out.push(n);
                }
            } else {
                tracing::warn!("Workspace index {} out of range 1..=9; ignoring", n);
            }
        } else {
            tracing::warn!(
                "Invalid workspace entry '{}'; ignoring",
                log_text::text(trimmed)
            );
        }
    }
}

fn deserialize_split<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SplitHelper {
        Single(String),
        List(Vec<String>),
        NumList(Vec<f64>),
    }

    let helper = SplitHelper::deserialize(deserializer)?;
    match helper {
        SplitHelper::Single(s) => {
            let list: Vec<String> = s
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
            if list.is_empty() {
                Ok(default_row_split())
            } else {
                Ok(list)
            }
        }
        SplitHelper::List(list) => Ok(list),
        SplitHelper::NumList(nums) => Ok(nums.into_iter().map(|n| n.to_string()).collect()),
    }
}

fn default_width_presets() -> Vec<f64> {
    vec![0.333, 0.5, 0.667]
}

fn default_width_preset() -> usize {
    1
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
            center_past_edges: false,
            center_columns: CenterColumnsSetting::default(),
            mouse_resize: MouseResize::default(),
            offscreen_mode: OffscreenMode::default(),
            width_presets: default_width_presets(),
            default_width_preset: default_width_preset(),
            height_presets: default_height_presets(),
            rows: RowsConfig::default(),
            outer_gap: None,
            default_column_width: None,
            min_column_width: None,
            max_column_width: None,
        }
    }
}

impl LayoutConfig {
    /// Select the 1-based default preset; fall back to the first, or 0.5 if empty.
    pub fn default_width_fraction(&self) -> f64 {
        let idx = self.default_width_preset.saturating_sub(1);
        self.width_presets
            .get(idx)
            .or_else(|| self.width_presets.first())
            .copied()
            .unwrap_or(0.5)
    }

    /// Convert the selected fraction using usable viewport width plus one gap,
    /// then subtract the gap, round down, and enforce a 100 px minimum.
    /// The result is independent of column count.
    pub fn default_column_width_px(&self, viewport_width: i32) -> i32 {
        let base = viewport_width
            .saturating_sub(self.outer_gap_left.max(0))
            .saturating_sub(self.outer_gap_right.max(0))
            .saturating_add(self.gap.max(0));
        let gap = self.gap.max(0);
        let frac = self.default_width_fraction();
        (base as f64 * frac - gap as f64).floor().max(100.0) as i32
    }

    /// Migrate legacy `outer_gap` field to per-side fields if present.
    /// Called after deserialization.
    pub fn migrate_outer_gap(&mut self) {
        if let Some(og) = self.outer_gap.take() {
            let og = og.max(0);
            // Preserve per-side customization; migrate only when all sides equal defaults.
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CenterColumnsConfig {
    #[default]
    Off,
    Single,
    Fit,
}

impl From<CenterColumnsConfig> for leopardwm_core_layout::CenterColumns {
    fn from(mode: CenterColumnsConfig) -> Self {
        match mode {
            CenterColumnsConfig::Off => Self::Off,
            CenterColumnsConfig::Single => Self::Single,
            CenterColumnsConfig::Fit => Self::Fit,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CenterColumnsSetting {
    pub center_columns: CenterColumnsConfig,
}

impl<'de> Deserialize<'de> for CenterColumnsSetting {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Values {
            center_columns: Option<CenterColumnsConfig>,
            center_single_column: bool,
        }
        let values = Values::deserialize(deserializer)?;
        Ok(Self {
            center_columns: values
                .center_columns
                .unwrap_or(if values.center_single_column {
                    CenterColumnsConfig::Single
                } else {
                    CenterColumnsConfig::Off
                }),
        })
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MouseResize {
    #[default]
    Presets,
    Free,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OffscreenMode {
    #[default]
    Bleed,
    Park,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CenteringModeConfig {
    /// Center the focused column in the viewport.
    #[default]
    Center,
    /// Only scroll if the focused column would be outside the viewport.
    JustInView,
    /// Center only when the focused column is wider than the viewport;
    /// otherwise behave like `JustInView`.
    OnOverflow,
}

impl From<CenteringModeConfig> for CenteringMode {
    fn from(config: CenteringModeConfig) -> Self {
        match config {
            CenteringModeConfig::Center => CenteringMode::Center,
            CenteringModeConfig::JustInView => CenteringMode::JustInView,
            CenteringModeConfig::OnOverflow => CenteringMode::OnOverflow,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    pub language: String,
    /// Whether to highlight the active window with a border overlay.
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

    /// Tab strip height in pixels at 96 DPI (DPI-scaled at render).
    #[serde(default = "default_tab_strip_height")]
    pub tab_strip_height: u32,

    /// Tab strip background color as hex RGB (e.g., "1F1F1F").
    #[serde(default = "default_tab_strip_bg")]
    pub tab_strip_bg: String,

    /// Active-tab background color as hex RGB.
    #[serde(default = "default_tab_strip_active_bg")]
    pub tab_strip_active_bg: String,

    /// Active-tab text color as hex RGB.
    #[serde(default = "default_tab_strip_active_text")]
    pub tab_strip_active_text: String,

    /// Inactive-tab text color as hex RGB.
    #[serde(default = "default_tab_strip_inactive_text")]
    pub tab_strip_inactive_text: String,

    /// Tab strip opacity (0..=255). 255 is fully opaque; default ~90%.
    #[serde(default = "default_tab_strip_opacity")]
    pub tab_strip_opacity: u8,
}

fn default_tab_strip_height() -> u32 {
    28
}
fn default_tab_strip_bg() -> String {
    "1F1F1F".to_string()
}
fn default_tab_strip_active_bg() -> String {
    "303030".to_string()
}
fn default_tab_strip_active_text() -> String {
    "FFFFFF".to_string()
}
fn default_tab_strip_inactive_text() -> String {
    "A0A0A0".to_string()
}
fn default_tab_strip_opacity() -> u8 {
    230
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            language: "en".to_string(),
            active_border: true,
            active_border_color: default_active_border_color(),
            active_border_width: default_active_border_width(),
            active_border_position: default_active_border_position(),
            tab_strip_height: default_tab_strip_height(),
            tab_strip_bg: default_tab_strip_bg(),
            tab_strip_active_bg: default_tab_strip_active_bg(),
            tab_strip_active_text: default_tab_strip_active_text(),
            tab_strip_inactive_text: default_tab_strip_inactive_text(),
            tab_strip_opacity: default_tab_strip_opacity(),
        }
    }
}

/// Default action for the implicit "close tab" gestures (X-button click,
/// middle-click). The right-click menu items always carry their literal
/// action and never consult this toggle.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TabCloseAction {
    #[default]
    CloseWindow,
    Untab,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskbarButtons {
    #[default]
    HideOffscreen,
    HideInactiveWorkspaces,
    ShowAll,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceEdgeWrap {
    #[default]
    Off,
    Wrap,
    Clamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, remote = "Self")]
pub struct BehaviorConfig {
    /// Whether to focus new windows automatically.
    #[serde(default = "default_true")]
    pub focus_new_windows: bool,

    pub new_window_monitor: NewWindowMonitor,

    pub fixed_size_windows: FixedSizeWindows,

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

    #[serde(default)]
    pub keep_awake: bool,

    /// Delay in milliseconds before focus changes on mouse enter.
    /// Only applies when focus_follows_mouse is true.
    #[serde(default = "default_focus_delay")]
    pub focus_follows_mouse_delay_ms: u32,

    /// Whether to disable Windows 11 Snap Layouts for tiled windows.
    /// Removes WS_MAXIMIZEBOX from managed tiled windows to prevent edge-drag
    /// snapping and the snap layout flyout. Restored when windows leave tiling.
    #[serde(default = "default_true")]
    pub disable_snap_layouts: bool,

    /// Whether to check GitHub Releases once a day for a newer version.
    /// Single anonymous HTTPS GET to api.github.com; no other telemetry.
    #[serde(default = "default_true")]
    pub check_for_updates: bool,

    /// Default action for implicit "close tab" gestures (X-button click,
    /// middle-click on a tab). The right-click menu items are unaffected.
    #[serde(default)]
    pub tab_close_action: TabCloseAction,

    /// Use DWM thumbnails to animate Chromium / Electron / Mozilla /
    /// Cascadia windows during layout transitions instead of per-frame
    /// `SetWindowPos` on the live HWND. Eliminates the visible 1px
    /// wobble and Chrome stutter during column scroll.
    #[serde(default = "default_true")]
    pub swap_chain_ghost_animation: bool,

    #[serde(default)]
    pub floating_above_tiled: bool,

    /// Where newly opened windows go: their own new column (default) or
    /// stacked into the focused column.
    #[serde(default)]
    pub new_window_placement: NewWindowPlacement,

    #[serde(default)]
    pub plain_drag_action: DragAction,

    /// Taskbar button visibility policy. Inactive workspaces hide all buttons
    /// unless ShowAll is selected. On the active workspace, HideOffscreen
    /// keeps buttons for visible, floating, and minimized windows.
    #[serde(default)]
    pub taskbar_buttons: TaskbarButtons,

    /// Vertical focus and moves at a column edge stay put (off), switch
    /// workspaces with wrapping (wrap), or stop at the ends (clamp).
    #[serde(default)]
    pub workspace_edge_wrap: WorkspaceEdgeWrap,

    #[serde(skip)]
    pub(crate) invalid_workspace_edge_wrap: Option<String>,

    /// Warp the mouse cursor onto the focused window after a focus-navigation
    /// command (focus left/right/up/down/next/prev/start/end and the monitor
    /// focus/move commands). Off by default.
    #[serde(default = "default_false")]
    pub mouse_follows_focus: bool,

    /// When a window is fullscreen, carry fullscreen to the newly focused
    /// window on a focus command (monocle mode) instead of dropping back to the
    /// tiled layout. On by default. Turn off so fullscreen only ever affects
    /// the one window it was toggled on.
    #[serde(default = "default_true")]
    pub fullscreen_follows_focus: bool,

    #[serde(default = "default_false")]
    pub skip_empty_workspaces: bool,

    #[serde(default = "default_false")]
    pub follow_window_to_workspace: bool,

    #[serde(default)]
    pub focus_crosses_monitors: bool,

    #[serde(default)]
    pub move_crosses_monitors: bool,

    #[serde(default = "default_move_step")]
    pub move_step: i32,

    /// Move a window into the adjacent row when moving up/down at its column edge.
    /// Default: false (no-op, current behavior).
    #[serde(default = "default_false")]
    pub move_window_across_rows: bool,
}

fn default_move_step() -> i32 {
    50
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FixedSizeWindows {
    #[default]
    Float,
    Tile,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NewWindowMonitor {
    #[default]
    Opening,
    Focused,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NewWindowPlacement {
    /// Each new window opens as its own column to the right of the focused
    /// column (scroll-tiling default; never resizes neighbors).
    #[default]
    NewColumn,
    /// New windows stack into the focused column.
    InColumn,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DragAction {
    #[default]
    Merge,
    Reorder,
}

impl DragAction {
    pub(crate) fn reorders(self, shift_held: bool) -> bool {
        (self == Self::Reorder) != shift_held
    }
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            focus_new_windows: true,
            new_window_monitor: NewWindowMonitor::default(),
            fixed_size_windows: FixedSizeWindows::default(),
            track_focus_changes: true,
            log_level: default_log_level(),
            focus_follows_mouse: false,
            keep_awake: false,
            focus_follows_mouse_delay_ms: default_focus_delay(),
            disable_snap_layouts: true,
            check_for_updates: true,
            tab_close_action: TabCloseAction::default(),
            swap_chain_ghost_animation: true,
            floating_above_tiled: false,
            new_window_placement: NewWindowPlacement::default(),
            plain_drag_action: DragAction::default(),
            taskbar_buttons: TaskbarButtons::default(),
            workspace_edge_wrap: WorkspaceEdgeWrap::Off,
            invalid_workspace_edge_wrap: None,
            mouse_follows_focus: false,
            fullscreen_follows_focus: true,
            skip_empty_workspaces: false,
            follow_window_to_workspace: false,
            focus_crosses_monitors: false,
            move_crosses_monitors: false,
            move_step: default_move_step(),
            move_window_across_rows: false,
        }
    }
}

impl Serialize for BehaviorConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Self::serialize(self, serializer)
    }
}

impl<'de> Deserialize<'de> for BehaviorConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        let mut invalid_workspace_edge_wrap = None;
        if let Some(fields) = value.as_object_mut() {
            if let Some(edge) = fields.get_mut("workspace_edge_wrap") {
                match edge {
                    serde_json::Value::Bool(enabled) => {
                        *edge =
                            serde_json::Value::String(if *enabled { "wrap" } else { "off" }.into());
                    }
                    serde_json::Value::String(mode)
                        if matches!(mode.as_str(), "off" | "wrap" | "clamp") => {}
                    _ => {
                        invalid_workspace_edge_wrap = Some(edge.to_string());
                        fields.remove("workspace_edge_wrap");
                    }
                }
            }
            if let Some(legacy) = fields.remove("hide_offscreen_taskbar_buttons") {
                if !fields.contains_key("taskbar_buttons") {
                    let enabled = legacy.as_bool().ok_or_else(|| {
                        serde::de::Error::custom("hide_offscreen_taskbar_buttons must be a boolean")
                    })?;
                    fields.insert(
                        "taskbar_buttons".into(),
                        serde_json::Value::String(
                            if enabled {
                                "hide_offscreen"
                            } else {
                                "show_all"
                            }
                            .into(),
                        ),
                    );
                }
            }
        }
        let mut config = Self::deserialize(value).map_err(serde::de::Error::custom)?;
        config.invalid_workspace_edge_wrap = invalid_workspace_edge_wrap;
        Ok(config)
    }
}

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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

    /// Maximum outer window size in logical pixels at admission.
    #[serde(default)]
    pub match_max_width: Option<i32>,
    #[serde(default)]
    pub match_max_height: Option<i32>,

    /// Action to take when the rule matches.
    #[serde(default)]
    pub action: WindowAction,

    /// Fixed width for floating windows (optional).
    #[serde(default)]
    pub width: Option<i32>,

    /// Fixed height for floating windows (optional).
    #[serde(default)]
    pub height: Option<i32>,

    /// Floating window's left offset from the opening monitor's work-area left edge,
    /// in 96-DPI logical pixels, scaled by that monitor's DPI. If either offset
    /// is set, both axes are clamped into that work area, with oversized
    /// dimensions aligned to its start.
    #[serde(default)]
    pub x: Option<i32>,

    /// Floating window's top offset from the opening monitor's work-area top edge,
    /// in 96-DPI logical pixels. An unset axis retains the window's own position
    /// before the work-area clamp.
    #[serde(default)]
    pub y: Option<i32>,

    /// `None` = auto-detect from DWM corner preference.
    #[serde(default)]
    pub corner_style: Option<CornerStyle>,

    #[serde(default)]
    pub border: Option<bool>,
    #[serde(default)]
    pub border_color: Option<String>,

    /// Open the window on this workspace (1-9) instead of the active one.
    #[serde(default)]
    pub open_on_workspace: Option<u8>,

    /// Maximize the window's column to the viewport width after opening.
    #[serde(default)]
    pub open_maximized: bool,

    /// Initial column width as a fraction of the viewport (0.05 to 1.0)
    /// for tiled windows.
    #[serde(default)]
    pub column_width: Option<f64>,

    /// Open the window at this 1-based column slot as its own column. Slots
    /// past the end append; 0 is ignored. Tiled windows only.
    #[serde(default)]
    pub open_in_column: Option<u8>,

    /// Open the window on this row (1-based, 1-4). Clamped to workspace row
    /// count; 0 is ignored; ignored if action is float. Tiled windows only.
    #[serde(default)]
    pub row: Option<u8>,

    /// Make the window sticky on open so it follows across workspaces. Pair
    /// with `action = "float"` for a floating overlay; on its own the window
    /// stays tiled and follows as a column. Opens on the active workspace.
    #[serde(default)]
    pub sticky: bool,
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

/// Border corner style override. Auto-detection (the default) reads each
/// window's actual `DWMWA_WINDOW_CORNER_PREFERENCE` so the border tracks
/// what Windows itself draws. Use this to force a specific style for an app
/// that misreports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CornerStyle {
    /// Square corners (radius = 0 px).
    Square,
    /// Standard Win11 rounded corners (radius = 8 px).
    Rounded,
    /// Smaller Win11 rounding (radius = 4 px).
    SmallRounded,
}

impl CornerStyle {
    pub fn radius_px(self) -> f32 {
        match self {
            CornerStyle::Square => 0.0,
            CornerStyle::Rounded => 8.0,
            CornerStyle::SmallRounded => 4.0,
        }
    }
}

/// Hotkey bindings configuration.
///
/// Each key is a hotkey string (e.g., "Ctrl+Alt+H") and each value is a command
/// (e.g., "focus_left"). Available actions and labels come from
/// `leopardwm_ipc::hotkeys::hotkey_catalog`.
/// Unbound actions include focus_next/focus_prev (linear window navigation)
/// scroll_left/scroll_right (move the strip viewport by 100 px),
/// nudge_left/nudge_right (move windows by the configured step), and
/// align_column_left/align_column_right (align to a work-area edge).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    /// Modifier keys required for scroll wheel navigation (e.g., "Ctrl+Alt").
    #[serde(default = "default_scroll_modifier")]
    pub scroll_modifier: String,

    /// Commands the user has intentionally left unbound. The defaults-merge
    /// skips re-adding their default binding, so a cleared binding stays
    /// cleared instead of springing back on the next load.
    #[serde(default)]
    pub disabled: Vec<String>,

    /// Map of hotkey string to command name.
    #[serde(flatten)]
    pub bindings: HashMap<String, String>,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            scroll_modifier: default_scroll_modifier(),
            disabled: Vec::new(),
            bindings: leopardwm_ipc::hotkeys::default_bindings_map(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GestureConfig {
    /// Whether gesture support is enabled. Takes effect on config reload.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Detect swipes delivered as injected wheel events by touchpad or mouse drivers.
    #[serde(default = "default_true")]
    pub wheel_swipes: bool,

    /// Use native Precision Touchpad Raw Input for three-finger swipes.
    /// Takes effect on daemon restart; the wheel hook remains the fallback.
    #[serde(default, skip_serializing_if = "is_false_bool")]
    pub raw_input: bool,

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

    /// Startup-only bounded diagnostic capture duration in seconds.
    /// `0` disables capture (default). Clamped to [`MAX_DIAGNOSTIC_CAPTURE_SECS`].
    /// Reload does not start or stop a capture; restart with `lwm stop` then `lwm run`.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub diagnostic_capture_secs: u64,
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

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn is_false_bool(value: &bool) -> bool {
    !*value
}

/// Hard maximum for `[gestures] diagnostic_capture_secs`.
pub const MAX_DIAGNOSTIC_CAPTURE_SECS: u64 = 120;

/// Clamp a requested capture duration. Used at logging init (before validate)
/// and again in [`Config::validate`] so the bound cannot be bypassed.
pub fn clamp_diagnostic_capture_secs(secs: u64) -> u64 {
    secs.min(MAX_DIAGNOSTIC_CAPTURE_SECS)
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            wheel_swipes: true,
            raw_input: false,
            swipe_left: default_swipe_left(),
            swipe_right: default_swipe_right(),
            swipe_up: default_swipe_up(),
            swipe_down: default_swipe_down(),
            scroll_up: default_scroll_up(),
            scroll_down: default_scroll_down(),
            diagnostic_capture_secs: 0,
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

/// Animation timing configuration.
///
/// Durations are in milliseconds; 0 means snap instantly (no animation).
/// Values are clamped to `[0, MAX_ANIMATION_DURATION_MS]` during validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnimationConfig {
    /// Column move / resize / tab-change transitions.
    #[serde(default = "default_layout_duration")]
    pub layout_duration_ms: u64,

    /// Workspace switch transitions (intentionally a touch slower).
    #[serde(default = "default_workspace_switch_duration")]
    pub workspace_switch_duration_ms: u64,

    /// Scroll animations (column-into-view, center, maximize).
    #[serde(default = "default_scroll_duration")]
    pub scroll_duration_ms: u64,

    /// Overview open/close zoom.
    #[serde(default = "default_overview_duration")]
    pub overview_duration_ms: u64,

    /// Easing curve applied to all of the above.
    #[serde(default)]
    pub easing: leopardwm_core_layout::Easing,

    /// Reduce motion (skip animations) while on battery or in Windows power
    /// saver. On by default to save power; set `false` to keep animations
    /// running on battery. The Windows "show animations" accessibility setting
    /// is always honored regardless of this.
    #[serde(default = "default_reduce_motion_on_battery")]
    pub reduce_motion_on_battery: bool,
}

/// Upper bound for any configured animation duration (ms). Guards against
/// a typo making the WM feel frozen.
pub const MAX_ANIMATION_DURATION_MS: u64 = 2000;

fn default_layout_duration() -> u64 {
    150
}

fn default_workspace_switch_duration() -> u64 {
    200
}

fn default_scroll_duration() -> u64 {
    200
}

fn default_overview_duration() -> u64 {
    150
}

fn default_reduce_motion_on_battery() -> bool {
    true
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            layout_duration_ms: default_layout_duration(),
            workspace_switch_duration_ms: default_workspace_switch_duration(),
            scroll_duration_ms: default_scroll_duration(),
            overview_duration_ms: default_overview_duration(),
            easing: leopardwm_core_layout::Easing::default(),
            reduce_motion_on_battery: default_reduce_motion_on_battery(),
        }
    }
}

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
    pub match_max_width: Option<i32>,
    pub match_max_height: Option<i32>,
    /// Action to take when the rule matches.
    pub action: WindowAction,
    /// Fixed width for floating windows (optional).
    pub width: Option<i32>,
    /// Fixed height for floating windows (optional).
    pub height: Option<i32>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    /// Optional corner-style override for the focus border.
    pub corner_style: Option<CornerStyle>,
    pub border_hidden: bool,
    pub border_color_bgr: Option<u32>,
    /// Open on this workspace (0-based index) instead of the active one.
    pub open_on_workspace: Option<usize>,
    /// Maximize the window's column after opening.
    pub open_maximized: bool,
    /// Initial column width as a viewport fraction for tiled windows.
    pub column_width: Option<f64>,
    /// Open at this 0-based column slot as its own column (validated from the
    /// 1-based config; high values append, 0 is dropped).
    pub open_in_column: Option<usize>,
    /// Open on this 0-based row slot (validated from 1-based config;
    /// clamped to workspace row count on insert, 0 is dropped). Tiled only.
    pub row: Option<usize>,
    /// Make the window sticky on open.
    pub sticky: bool,
}

fn matches_window_size(
    max_width: Option<i32>,
    max_height: Option<i32>,
    size: Option<(i32, i32)>,
) -> bool {
    if max_width.is_none() && max_height.is_none() {
        return true;
    }
    let Some((width, height)) = size else {
        return false;
    };
    max_width.is_none_or(|max| width <= max) && max_height.is_none_or(|max| height <= max)
}

impl CompiledWindowRule {
    pub(crate) fn has_size_condition(&self) -> bool {
        self.match_max_width.is_some() || self.match_max_height.is_some()
    }

    pub fn matches(
        &self,
        class_name: &str,
        title: &str,
        executable: &str,
        size: Option<(i32, i32)>,
    ) -> bool {
        let has_any_criteria = self.class_regex.is_some()
            || self.title_regex.is_some()
            || self.match_executable.is_some();

        if !has_any_criteria
            || !matches_window_size(self.match_max_width, self.match_max_height, size)
        {
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
/// Returns None if the command is not recognized. Accepts hyphenated
/// variants (e.g. "panic-revert") by normalizing to underscores before
/// the shared catalog lookup.
pub fn parse_command(cmd: &str) -> Option<leopardwm_ipc::IpcCommand> {
    leopardwm_ipc::hotkeys::command_for_action(&canonical_action_id(cmd))
}

/// Normalize an action identifier and apply the same renames as config migration.
/// Keep the loaded spelling separately when reporting configuration issues.
pub(crate) fn canonical_action_id(action: &str) -> String {
    let normalized = action.to_lowercase().replace('-', "_");
    HOTKEY_COMMAND_RENAMES
        .iter()
        .find_map(|(old, new)| (normalized == *old).then(|| (*new).to_string()))
        .unwrap_or(normalized)
}

const DEPRECATED_HOTKEY_COMMANDS: &[&str] = &["width_third", "width_half", "width_two_thirds"];

const HOTKEY_COMMAND_RENAMES: &[(&str, &str)] = &[
    ("resize_grow", "cycle_width_up"),
    ("resize_shrink", "cycle_width_down"),
];

impl Config {
    fn migrate_hotkey_bindings(bindings: &mut HashMap<String, String>) {
        for (old, new) in HOTKEY_COMMAND_RENAMES {
            for value in bindings.values_mut() {
                if value == old {
                    *value = new.to_string();
                }
            }
        }
        bindings.retain(|_, cmd| !DEPRECATED_HOTKEY_COMMANDS.contains(&cmd.as_str()));
    }

    /// Load the first existing path from [`config_paths`], or defaults if none exists.
    pub fn load() -> Result<Self> {
        CONFIG_FILE.lock().unwrap().load(&config_paths())
    }

    /// Validate configuration values, clamping out-of-range fields and returning warnings.
    pub fn validate(&mut self) -> Vec<ConfigWarning> {
        let mut warnings = Vec::new();
        if !crate::locale::is_supported(&self.appearance.language) {
            warnings.push(ConfigWarning {
                field: "appearance.language".to_string(),
                message: format!(
                    "Unsupported language '{}', using English",
                    self.appearance.language
                ),
            });
            self.appearance.language = "en".to_string();
        }

        if !(1..=1000).contains(&self.behavior.move_step) {
            warnings.push(ConfigWarning {
                field: "behavior.move_step".to_string(),
                message: format!(
                    "move_step ({}) out of range 1..=1000, clamped",
                    self.behavior.move_step
                ),
            });
            self.behavior.move_step = self.behavior.move_step.clamp(1, 1000);
        }

        if self.behavior.floating_above_tiled && !self.behavior.track_focus_changes {
            warnings.push(ConfigWarning {
                field: "behavior.floating_above_tiled".to_string(),
                message: "floating_above_tiled has no effect without focus tracking; enable track_focus_changes"
                    .to_string(),
            });
        }

        // animation durations clamped to a sane ceiling so a typo can't
        // make the WM feel frozen.
        for (field, val) in [
            (
                "animation.layout_duration_ms",
                &mut self.animation.layout_duration_ms,
            ),
            (
                "animation.workspace_switch_duration_ms",
                &mut self.animation.workspace_switch_duration_ms,
            ),
            (
                "animation.scroll_duration_ms",
                &mut self.animation.scroll_duration_ms,
            ),
            (
                "animation.overview_duration_ms",
                &mut self.animation.overview_duration_ms,
            ),
        ] {
            if *val > MAX_ANIMATION_DURATION_MS {
                warnings.push(ConfigWarning {
                    field: field.to_string(),
                    message: format!(
                        "{} ({}) exceeds max, clamped to {}",
                        field, *val, MAX_ANIMATION_DURATION_MS
                    ),
                });
                *val = MAX_ANIMATION_DURATION_MS;
            }
        }

        if self.layout.gap < 0 {
            warnings.push(ConfigWarning {
                field: "layout.gap".to_string(),
                message: format!("Negative gap ({}) clamped to 0", self.layout.gap),
            });
            self.layout.gap = 0;
        }

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

        // row gap must be >= 0
        if self.layout.rows.gap < 0 {
            warnings.push(ConfigWarning {
                field: "layout.rows.gap".to_string(),
                message: format!("Negative row gap ({}) clamped to 0", self.layout.rows.gap),
            });
            self.layout.rows.gap = 0;
        }

        for (idx, rule) in self.window_rules.iter_mut().enumerate() {
            match rule.row {
                Some(_) if rule.action == WindowAction::Float => {
                    warnings.push(ConfigWarning {
                        field: format!("window_rules[{}].row", idx),
                        message: "row is not applicable with action = \"float\"; ignoring"
                            .to_string(),
                    });
                    rule.row = None;
                }
                Some(0) => {
                    warnings.push(ConfigWarning {
                        field: format!("window_rules[{}].row", idx),
                        message: "row = 0 is invalid (1-based, 1..=4); ignoring".to_string(),
                    });
                    rule.row = None;
                }
                Some(n) if (n as usize) > leopardwm_core_layout::MAX_ROWS => {
                    warnings.push(ConfigWarning {
                        field: format!("window_rules[{}].row", idx),
                        message: format!(
                            "row ({}) exceeds max {}, clamped to {}",
                            n,
                            leopardwm_core_layout::MAX_ROWS,
                            leopardwm_core_layout::MAX_ROWS
                        ),
                    });
                    rule.row = Some(leopardwm_core_layout::MAX_ROWS as u8);
                }
                _ => {}
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

        let preset_count = self.layout.width_presets.len();
        if self.layout.default_width_preset == 0 || self.layout.default_width_preset > preset_count
        {
            warnings.push(ConfigWarning {
                field: "layout.default_width_preset".to_string(),
                message: format!(
                    "default_width_preset ({}) out of range 1..={}, using 1",
                    self.layout.default_width_preset, preset_count
                ),
            });
            self.layout.default_width_preset = 1;
        }

        if self.layout.height_presets.is_empty() {
            warnings.push(ConfigWarning {
                field: "layout.height_presets".to_string(),
                message: "Empty height_presets, using defaults".to_string(),
            });
            self.layout.height_presets = default_height_presets();
        }

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

        for (index, rule) in self.window_rules.iter_mut().enumerate() {
            if let Some(color) = &rule.border_color {
                if color.len() != 6 || !color.chars().all(|c| c.is_ascii_hexdigit()) {
                    warnings.push(ConfigWarning {
                        field: format!("window_rules[{index}].border_color"),
                        message: format!(
                            "Invalid hex color '{}' (must be 6 hex chars, e.g. \"4285F4\"), reset to default",
                            color
                        ),
                    });
                    rule.border_color = None;
                }
            }
        }

        // Invalid strip heights reset to the default so a typo cannot hide the
        // strip or make it consume the screen. Colors require six hex digits.
        for (field, value, default_fn) in [
            (
                "appearance.tab_strip_bg",
                &self.appearance.tab_strip_bg.clone(),
                default_tab_strip_bg as fn() -> String,
            ),
            (
                "appearance.tab_strip_active_bg",
                &self.appearance.tab_strip_active_bg.clone(),
                default_tab_strip_active_bg as fn() -> String,
            ),
            (
                "appearance.tab_strip_active_text",
                &self.appearance.tab_strip_active_text.clone(),
                default_tab_strip_active_text as fn() -> String,
            ),
            (
                "appearance.tab_strip_inactive_text",
                &self.appearance.tab_strip_inactive_text.clone(),
                default_tab_strip_inactive_text as fn() -> String,
            ),
        ] {
            let is_valid = value.len() == 6 && value.chars().all(|c| c.is_ascii_hexdigit());
            if !is_valid {
                warnings.push(ConfigWarning {
                    field: field.to_string(),
                    message: format!(
                        "Invalid hex color '{}' (must be 6 hex chars, e.g. \"1F1F1F\"), reset to default",
                        value
                    ),
                });
                let default = default_fn();
                match field {
                    "appearance.tab_strip_bg" => self.appearance.tab_strip_bg = default,
                    "appearance.tab_strip_active_bg" => {
                        self.appearance.tab_strip_active_bg = default
                    }
                    "appearance.tab_strip_active_text" => {
                        self.appearance.tab_strip_active_text = default
                    }
                    "appearance.tab_strip_inactive_text" => {
                        self.appearance.tab_strip_inactive_text = default
                    }
                    _ => {}
                }
            }
        }
        if !(16..=64).contains(&self.appearance.tab_strip_height) {
            warnings.push(ConfigWarning {
                field: "appearance.tab_strip_height".to_string(),
                message: format!(
                    "tab_strip_height must be between 16 and 64 px (got {}), reset to default",
                    self.appearance.tab_strip_height
                ),
            });
            self.appearance.tab_strip_height = default_tab_strip_height();
        }

        if let Some(value) = self.behavior.invalid_workspace_edge_wrap.take() {
            warnings.push(ConfigWarning {
                field: "behavior.workspace_edge_wrap".to_string(),
                message: format!("Invalid workspace_edge_wrap {value} (must be off/wrap/clamp or true/false), falling back to off"),
            });
        }

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

        {
            let requested = self.gestures.diagnostic_capture_secs;
            let clamped = clamp_diagnostic_capture_secs(requested);
            if clamped != requested {
                warnings.push(ConfigWarning {
                    field: "gestures.diagnostic_capture_secs".to_string(),
                    message: format!(
                        "gestures.diagnostic_capture_secs ({requested}) exceeds max, clamped to {clamped}"
                    ),
                });
                self.gestures.diagnostic_capture_secs = clamped;
            }
        }

        warnings
    }

    /// Compile window rules into pre-compiled regex patterns for efficient matching.
    ///
    /// Invalid regex patterns are logged as warnings and their rules are skipped.
    pub fn compile_window_rules(&self) -> Vec<CompiledWindowRule> {
        let mut compiled = Vec::new();

        for (index, rule) in self.window_rules.iter().enumerate() {
            let class_regex = match &rule.match_class {
                Some(pattern) => match regex::RegexBuilder::new(pattern)
                    .size_limit(1_000_000)
                    .build()
                {
                    Ok(re) => Some(re),
                    Err(_) => {
                        tracing::warn!(
                            "Window rule {}: invalid regex in match_class; skipping rule",
                            index + 1
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
                    Err(_) => {
                        tracing::warn!(
                            "Window rule {}: invalid regex in match_title; skipping rule",
                            index + 1
                        );
                        continue;
                    }
                },
                None => None,
            };

            // Validate the open placement extras; warn and drop just the
            // invalid property rather than the whole rule.
            let open_on_workspace = match rule.open_on_workspace {
                Some(n) if (1..=9).contains(&n) => Some((n - 1) as usize),
                Some(n) => {
                    tracing::warn!(
                        "Window rule open_on_workspace = {} is out of range (1-9); ignoring",
                        n
                    );
                    None
                }
                None => None,
            };
            let column_width = match rule.column_width {
                Some(f) if (0.05..=1.0).contains(&f) => Some(f),
                Some(f) => {
                    tracing::warn!(
                        "Window rule column_width = {} is out of range (0.05-1.0); ignoring",
                        f
                    );
                    None
                }
                None => None,
            };
            // 1-based slot -> 0-based index. High values append (clamped at
            // insert time); 0 violates the 1-based contract and is dropped.
            let open_in_column = match rule.open_in_column {
                Some(n) if n >= 1 => Some((n - 1) as usize),
                Some(_) => {
                    tracing::warn!("Window rule open_in_column = 0 is invalid (1-based); ignoring");
                    None
                }
                None => None,
            };
            // 1-based row -> 0-based index. Clamped at insert time; 0 is invalid and dropped.
            // Ignored with warning if action is float.
            let row = match rule.row {
                Some(_) if rule.action == WindowAction::Float => {
                    tracing::warn!(
                        "Window rule row is not applicable with action = \"float\"; ignoring"
                    );
                    None
                }
                Some(n) if n >= 1 => {
                    Some(((n as usize) - 1).min(leopardwm_core_layout::MAX_ROWS - 1))
                }
                Some(_) => {
                    tracing::warn!("Window rule row = 0 is invalid (1-based); ignoring");
                    None
                }
                None => None,
            };

            let positive_limit = |value: Option<i32>, field: &str| {
                value.filter(|&n| {
                    if n <= 0 {
                        tracing::warn!(
                            "Window rule {} = {} is invalid (must be positive); ignoring",
                            field,
                            n
                        );
                    }
                    n > 0
                })
            };
            let match_max_width = positive_limit(rule.match_max_width, "match_max_width");
            let match_max_height = positive_limit(rule.match_max_height, "match_max_height");

            compiled.push(CompiledWindowRule {
                class_regex,
                title_regex,
                match_executable: rule.match_executable.clone(),
                match_max_width,
                match_max_height,
                action: rule.action,
                width: rule.width,
                height: rule.height,
                x: rule.x,
                y: rule.y,
                corner_style: rule.corner_style,
                border_hidden: rule.border == Some(false),
                border_color_bgr: rule
                    .border_color
                    .as_deref()
                    .and_then(crate::ui_sync::hex_rgb_to_bgr),
                open_on_workspace,
                open_maximized: rule.open_maximized,
                column_width,
                open_in_column,
                row,
                sticky: rule.sticky,
            });
        }

        // Append built-in rules after user rules so users can override them.
        let builtin_rules = BUILTIN_IGNORE_EXECUTABLES
            .iter()
            .map(|&exe| (exe, None, WindowAction::Ignore))
            .chain(
                BUILTIN_FLOAT_RULES
                    .iter()
                    .map(|&(exe, title)| (exe, Some(title), WindowAction::Float)),
            );
        for (exe, title, action) in builtin_rules {
            compiled.push(CompiledWindowRule {
                class_regex: None,
                title_regex: title.map(|pattern| {
                    regex::RegexBuilder::new(pattern)
                        .size_limit(1_000_000)
                        .build()
                        .expect("invalid built-in window rule title regex")
                }),
                match_executable: Some(exe.to_string()),
                match_max_width: None,
                match_max_height: None,
                action,
                width: None,
                height: None,
                x: None,
                y: None,
                corner_style: None,
                border_hidden: false,
                border_color_bgr: None,
                open_on_workspace: None,
                open_maximized: false,
                column_width: None,
                open_in_column: None,
                row: None,
                sticky: false,
            });
        }

        compiled
    }

    #[cfg(test)]
    pub fn load_from_path(path: &PathBuf) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        Self::parse(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))
    }

    fn parse(content: &str) -> std::result::Result<Self, toml::de::Error> {
        let mut config: Config = toml::from_str(content)?;

        config.layout.migrate_outer_gap();

        Self::migrate_hotkey_bindings(&mut config.hotkeys.bindings);

        Self::merge_default_hotkeys(&mut config.hotkeys);

        Ok(config)
    }

    /// Re-add defaults unless their command is bound or disabled, or their
    /// chord is already bound by the user.
    fn merge_default_hotkeys(hotkeys: &mut HotkeyConfig) {
        let user_commands: HashSet<String> = hotkeys.bindings.values().cloned().collect();
        let user_chords: HashSet<_> = hotkeys
            .bindings
            .keys()
            .filter_map(|key| parse_hotkey_string(key))
            .collect();
        let disabled: HashSet<&String> = hotkeys.disabled.iter().collect();
        for (key, cmd) in HotkeyConfig::default().bindings {
            if !user_commands.contains(&cmd)
                && !disabled.contains(&cmd)
                && !parse_hotkey_string(&key).is_some_and(|chord| user_chords.contains(&chord))
            {
                hotkeys.bindings.insert(key, cmd);
            }
        }
    }

    /// Save configuration to the loaded path, or the primary path if none was loaded.
    pub fn save(&self) -> Result<()> {
        // Unit tests exercise command handlers that persist config; never
        // let them overwrite the developer's real config file.
        if cfg!(test) {
            return Ok(());
        }
        let result = CONFIG_FILE.lock().unwrap().save(self, &config_paths());
        if let Err(error) = &result {
            tracing::warn!(
                "Failed to save config: {}",
                log_text::text(format_args!("{error:#}"))
            );
            let message = if let Some(recovery) = error.downcast_ref::<HardLinkWriteError>() {
                format!(
                    "The settings were not saved. The original config is backed up at {}.",
                    recovery.backup.display()
                )
            } else {
                "The settings were not saved. config.toml may have changed outside LeopardWM, or no backup slot is free. Reload the config or move the .corrupt files, then try again.".to_string()
            };
            crate::notify::show_toast("Settings not saved", &message);
        }
        result
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ConfigRejection {
    pub path: PathBuf,
    pub reason: String,
    pub location: Option<(usize, usize)>,
    pub preserved_at: Option<PathBuf>,
    pub recovered_fields: Vec<recovery::DroppedField>,
    bytes: Option<Vec<u8>>,
    pending: bool,
}

#[derive(Default)]
struct ConfigFile {
    path: Option<PathBuf>,
    last_seen: Option<Vec<u8>>,
    rejections: Vec<ConfigRejection>,
    rejected_paths: Vec<PathBuf>,
    unknown_keys: Vec<String>,
}

static CONFIG_FILE: Mutex<ConfigFile> = Mutex::new(ConfigFile {
    path: None,
    last_seen: None,
    rejections: Vec::new(),
    rejected_paths: Vec::new(),
    unknown_keys: Vec::new(),
});

pub(crate) fn unknown_config_keys() -> Vec<String> {
    CONFIG_FILE.lock().unwrap().unknown_keys.clone()
}

pub(crate) fn config_rejection() -> Option<ConfigRejection> {
    CONFIG_FILE.lock().unwrap().rejections.last().cloned()
}

fn corrupt_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[derive(Debug, thiserror::Error)]
#[error("Failed to write hard-linked config; original copy kept at {}", backup.display())]
struct HardLinkWriteError {
    backup: PathBuf,
    #[source]
    source: std::io::Error,
}

fn replace_config_file(target: &Path, temp: &Path, content: &[u8]) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let source = match fs::File::open(target) {
        Ok(source) => source,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return fs::rename(temp, target).map_err(Into::into)
        }
        Err(error) => return Err(error.into()),
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(HANDLE(source.as_raw_handle()), &mut info)?;
    }
    if info.nNumberOfLinks > 1 {
        let mut destination = fs::OpenOptions::new().write(true).open(target)?;
        let backup = corrupt_path(target, &format!(".{}.backup", uuid::Uuid::new_v4()));
        let mut copy = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)?;
        let mut source = source;
        if let Err(error) = std::io::copy(&mut source, &mut copy).and_then(|_| copy.sync_all()) {
            drop(copy);
            let _ = fs::remove_file(&backup);
            return Err(error).with_context(|| {
                format!("Failed to back up hard-linked config: {}", backup.display())
            });
        }
        drop(copy);
        drop(source);
        if let Err(error) = destination.set_len(0) {
            let _ = fs::remove_file(&backup);
            return Err(error.into());
        }
        destination
            .write_all(content)
            .and_then(|()| destination.sync_all())
            .map_err(|source| HardLinkWriteError {
                backup: backup.clone(),
                source,
            })?;
        if let Err(error) = fs::remove_file(&backup) {
            tracing::warn!(
                "Could not remove config backup {}: {}",
                log_text::path(backup.display()),
                log_text::text(&error)
            );
        }
        if let Err(error) = fs::remove_file(temp) {
            tracing::warn!(
                "Could not remove temporary config {}: {}",
                log_text::path(temp.display()),
                log_text::text(&error)
            );
        }
        Ok(())
    } else {
        drop(source);
        fs::rename(temp, target).map_err(Into::into)
    }
}

fn preserve_config_bytes(path: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let mut free = None;
    for suffix in [".corrupt", ".corrupt.1"] {
        let copy = corrupt_path(path, suffix);
        match fs::read(&copy) {
            Ok(existing) if existing == bytes => return Ok(copy),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if free.is_none() {
                    free = Some(copy);
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to read config backup {}", copy.display()))
            }
        }
    }
    let copy = free.ok_or_else(|| {
        anyhow::anyhow!("No free backup slot for rejected config {}", path.display())
    })?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&copy)
        .with_context(|| format!("Failed to preserve config at {}", copy.display()))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&copy);
        return Err(error)
            .with_context(|| format!("Failed to preserve config at {}", copy.display()));
    }
    Ok(copy)
}

fn find_unknown_config_keys(
    content: &str,
    config: &Config,
    dropped: &[recovery::DroppedField],
) -> Vec<String> {
    fn walk(
        input: &toml::Value,
        output: &toml::Value,
        path: &str,
        pattern: &str,
        dropped: &[recovery::DroppedField],
        keys: &mut Vec<String>,
    ) {
        match (input, output) {
            (toml::Value::Table(input), toml::Value::Table(output)) => {
                for (key, value) in input {
                    let name = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    let canonical = if pattern.is_empty() {
                        key.clone()
                    } else {
                        format!("{pattern}.{key}")
                    };
                    if matches!(
                        canonical.as_str(),
                        "hotkeys"
                            | "layout.center_single_column"
                            | "layout.outer_gap"
                            | "layout.default_column_width"
                            | "layout.min_column_width"
                            | "layout.max_column_width"
                            | "behavior.hide_offscreen_taskbar_buttons"
                            | "layout.rows.workspaces[].workspace"
                            | "gestures.raw_input"
                            | "gestures.diagnostic_capture_secs"
                    ) {
                        continue;
                    }
                    if dropped.iter().any(|field| field.path == name) {
                        continue;
                    }
                    if let Some(other) = output.get(key) {
                        walk(value, other, &name, &canonical, dropped, keys);
                    } else {
                        keys.push(name);
                    }
                }
            }
            (toml::Value::Array(input), toml::Value::Array(output)) => {
                let mut retained = output.iter();
                for (index, value) in input.iter().enumerate() {
                    let name = format!("{path}[{index}]");
                    // Recovery already reports the whole dropped element.
                    if dropped.iter().any(|field| field.path == name) {
                        continue;
                    }
                    if let Some(other) = retained.next() {
                        walk(value, other, &name, &format!("{pattern}[]"), dropped, keys);
                    }
                }
            }
            _ => {}
        }
    }
    let Ok(input) = toml::from_str::<toml::Table>(content) else {
        return Vec::new();
    };
    let Ok(output) = toml::Value::try_from(config) else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    walk(
        &toml::Value::Table(input),
        &output,
        "",
        "",
        dropped,
        &mut keys,
    );
    keys
}

impl ConfigFile {
    fn record_unknown_keys(
        &mut self,
        content: &str,
        config: &Config,
        dropped: &[recovery::DroppedField],
    ) {
        self.unknown_keys = find_unknown_config_keys(content, config, dropped);
        for key in &self.unknown_keys {
            tracing::warn!("Unknown config key: {}", log_text::text(key));
        }
    }

    fn load(&mut self, paths: &[PathBuf]) -> Result<Config> {
        for path in paths {
            let bytes = match fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    self.rejections
                        .retain(|rejection| rejection.path != *path || rejection.bytes.is_some());
                    continue;
                }
                Err(error) => {
                    let reason = format!("Failed to read config file {}: {error}", path.display());
                    self.last_seen = None;
                    self.reject(path, reason.clone(), None, None);
                    return Err(anyhow::anyhow!(reason));
                }
            };
            self.rejections
                .retain(|rejection| rejection.path != *path || rejection.bytes.is_some());
            self.path = Some(path.clone());
            self.last_seen = Some(bytes.clone());
            let content = match std::str::from_utf8(&bytes) {
                Ok(content) => content,
                Err(error) => {
                    let reason = format!(
                        "Failed to read config file {} as UTF-8: {error}",
                        path.display()
                    );
                    self.reject(path, reason.clone(), None, Some(bytes));
                    return Err(anyhow::anyhow!(reason));
                }
            };
            match Config::parse(content) {
                Ok(config) => {
                    self.record_unknown_keys(content, &config, &[]);
                    self.rejections.clear();
                    tracing::info!("Loading config from: {}", log_text::path(path.display()));
                    return Ok(config);
                }
                Err(error) => {
                    if let Some((config, fields)) = recovery::recover(content) {
                        self.record_unknown_keys(content, &config, &fields);
                        for field in &fields {
                            tracing::warn!(
                                "Config: {} reset to default: {}",
                                log_text::path(&field.path),
                                log_text::text(&field.reason)
                            );
                        }
                        let reason = format!("Recovered config file {}: {error}", path.display());
                        let location = fields.first().and_then(|field| field.location);
                        self.reject(path, reason, location, Some(bytes));
                        self.rejections.last_mut().unwrap().recovered_fields = fields;
                        return Ok(config);
                    }
                    let location = error.span().map(|span| {
                        let prefix = &content[..span.start];
                        (
                            prefix.bytes().filter(|&byte| byte == b'\n').count() + 1,
                            prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
                        )
                    });
                    let reason = format!("Failed to parse config file {}: {error}", path.display());
                    self.reject(path, reason.clone(), location, Some(bytes));
                    return Err(anyhow::anyhow!(reason));
                }
            }
        }
        self.path = None;
        self.last_seen = None;
        self.rejections.clear();
        self.unknown_keys.clear();
        tracing::info!("No config file found, using defaults");
        Ok(Config::default())
    }

    fn reject(
        &mut self,
        path: &Path,
        reason: String,
        location: Option<(usize, usize)>,
        bytes: Option<Vec<u8>>,
    ) {
        self.path = Some(path.to_path_buf());
        if !self.rejected_paths.iter().any(|rejected| rejected == path) {
            self.rejected_paths.push(path.to_path_buf());
        }
        if let Some(index) = self
            .rejections
            .iter()
            .position(|r| r.path == path && r.bytes == bytes)
        {
            let mut rejection = self.rejections.remove(index);
            rejection.reason = reason;
            rejection.location = location;
            rejection.pending = true;
            rejection.recovered_fields.clear();
            self.rejections.push(rejection);
        } else {
            self.rejections.push(ConfigRejection {
                path: path.to_path_buf(),
                reason,
                location,
                preserved_at: None,
                recovered_fields: Vec::new(),
                bytes,
                pending: true,
            });
        }
    }

    fn save(&mut self, config: &Config, paths: &[PathBuf]) -> Result<()> {
        let path = self
            .path
            .as_ref()
            .or_else(|| paths.first())
            .ok_or_else(|| anyhow::anyhow!("No config path available"))?
            .clone();
        let content =
            toml::to_string_pretty(config).context("Failed to serialize config to TOML")?;
        for rejection in self
            .rejections
            .iter_mut()
            .filter(|rejection| rejection.pending)
        {
            let bytes = rejection.bytes.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Cannot preserve unreadable config file: {}",
                    rejection.path.display()
                )
            })?;
            rejection.preserved_at = Some(preserve_config_bytes(&rejection.path, bytes)?);
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create config directory: {}", parent.display())
            })?;
        }
        let target = match fs::canonicalize(&path) {
            Ok(target) => target,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                match fs::symlink_metadata(&path) {
                    Err(missing) if missing.kind() == ErrorKind::NotFound => path.clone(),
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        fs::write(&path, &content).with_context(|| {
                            format!("Failed to write config file: {}", path.display())
                        })?;
                        self.finish_save(path, content);
                        return Ok(());
                    }
                    _ => {
                        return Err(error).with_context(|| {
                            format!("Failed to resolve config file: {}", path.display())
                        })
                    }
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to resolve config file: {}", path.display()))
            }
        };
        let temp = corrupt_path(&target, &format!(".{}.tmp", uuid::Uuid::new_v4()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| {
                format!("Failed to create temporary config file: {}", temp.display())
            })?;
        let result = (|| -> Result<()> {
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            drop(file);
            if self.rejected_paths.contains(&path) {
                match fs::read(&target) {
                    Ok(current) => {
                        if self.last_seen.as_deref() != Some(current.as_slice()) {
                            preserve_config_bytes(&path, &current)?;
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "Failed to read config file before saving: {}",
                                path.display()
                            )
                        })
                    }
                }
            }
            replace_config_file(&target, &temp, content.as_bytes())?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result.with_context(|| format!("Failed to write config file {}", path.display()))?;
        self.finish_save(path, content);
        Ok(())
    }

    fn finish_save(&mut self, path: PathBuf, content: String) {
        self.unknown_keys.clear();
        for rejection in &mut self.rejections {
            rejection.pending = false;
        }
        tracing::info!("Config saved to: {}", log_text::path(path.display()));
        self.path = Some(path);
        self.last_seen = Some(content.into_bytes());
    }
}

/// Write the commented default config to disk if no config file exists.
/// Returns Ok(Some(path)) if a file was created, Ok(None) if it already exists.
pub fn ensure_config_on_disk() -> Result<Option<PathBuf>> {
    let paths = config_paths();
    for path in &paths {
        match fs::metadata(path) {
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to inspect config file: {}", path.display()))
            }
        }
    }
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

fn generate_default_config_content() -> String {
    leopardwm_ipc::config_template::render_default_config()
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

    paths.push(PathBuf::from("config.toml"));

    paths
}

fn dirs_home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_drag_action_defaults_and_round_trips() {
        let omitted: Config = toml::from_str("[behavior]").unwrap();
        assert_eq!(omitted.behavior.plain_drag_action, DragAction::Merge);
        for (name, action) in [
            ("merge", DragAction::Merge),
            ("reorder", DragAction::Reorder),
        ] {
            let config: Config =
                toml::from_str(&format!("[behavior]\nplain_drag_action = \"{name}\"")).unwrap();
            assert_eq!(config.behavior.plain_drag_action, action);
            let serialized = toml::to_string(&config).unwrap();
            let reloaded: Config = toml::from_str(&serialized).unwrap();
            assert_eq!(reloaded.behavior.plain_drag_action, action);
        }
    }

    #[test]
    fn glazewm_compat_defaults_and_explicit_config_round_trip() {
        let omitted: Config = toml::from_str("").unwrap();
        assert_eq!(
            omitted.glazewm_compat,
            GlazewmCompatConfig {
                enabled: false,
                port: 6123
            }
        );
        let partial: Config = toml::from_str("[glazewm_compat]\nenabled = true").unwrap();
        assert_eq!(
            partial.glazewm_compat,
            GlazewmCompatConfig {
                enabled: true,
                port: 6123
            }
        );
        let configured: Config =
            toml::from_str("[glazewm_compat]\nenabled = true\nport = 17000").unwrap();
        let restored: Config = toml::from_str(&toml::to_string(&configured).unwrap()).unwrap();
        assert_eq!(restored.glazewm_compat, configured.glazewm_compat);
    }

    #[test]
    fn workspace_edge_wrap_config_compatibility_and_validation() {
        for (input, expected, saved) in [
            ("true", WorkspaceEdgeWrap::Wrap, "wrap"),
            ("false", WorkspaceEdgeWrap::Off, "off"),
            ("\"wrap\"", WorkspaceEdgeWrap::Wrap, "wrap"),
            ("\"clamp\"", WorkspaceEdgeWrap::Clamp, "clamp"),
            ("\"off\"", WorkspaceEdgeWrap::Off, "off"),
        ] {
            let config: Config =
                toml::from_str(&format!("[behavior]\nworkspace_edge_wrap = {input}")).unwrap();
            assert_eq!(config.behavior.workspace_edge_wrap, expected);
            let serialized = toml::to_string(&config).unwrap();
            let value: toml::Value = toml::from_str(&serialized).unwrap();
            assert_eq!(
                value["behavior"]["workspace_edge_wrap"].as_str(),
                Some(saved)
            );
            let reloaded: Config = toml::from_str(&serialized).unwrap();
            assert_eq!(reloaded.behavior.workspace_edge_wrap, expected);
        }
        let omitted: Config = toml::from_str("[behavior]").unwrap();
        assert_eq!(omitted.behavior.workspace_edge_wrap, WorkspaceEdgeWrap::Off);
        assert!(!omitted.behavior.focus_crosses_monitors);
        assert!(!omitted.behavior.move_crosses_monitors);
        let enabled: Config = toml::from_str(
            "[behavior]\nfocus_crosses_monitors = true\nmove_crosses_monitors = true",
        )
        .unwrap();
        assert!(enabled.behavior.focus_crosses_monitors);
        assert!(enabled.behavior.move_crosses_monitors);
        for bad in ["\"sideways\"", "42"] {
            let mut config: Config =
                toml::from_str(&format!("[behavior]\nworkspace_edge_wrap = {bad}")).unwrap();
            assert_eq!(config.behavior.workspace_edge_wrap, WorkspaceEdgeWrap::Off);
            let warnings = config.validate();
            assert_eq!(warnings.len(), 1);
            assert_eq!(warnings[0].field, "behavior.workspace_edge_wrap");
            assert!(warnings[0].message.contains(bad));
            assert!(warnings[0].message.contains("off/wrap/clamp"));
            assert!(warnings[0].message.contains("true/false"));
            assert!(warnings[0].message.contains("falling back to off"));
            assert!(config.validate().is_empty());
        }
        let _: Config =
            toml::from_str(&leopardwm_ipc::config_template::render_default_config()).unwrap();
    }

    #[test]
    fn language_default_round_trip_and_validation() {
        let mut config = Config::default();
        assert_eq!(config.appearance.language, "en");
        let legacy: Config = toml::from_str("[appearance]\nactive_border = false").unwrap();
        assert_eq!(legacy.appearance.language, "en");
        let languages: serde_json::Value =
            serde_json::from_str(&crate::locale::languages_json()).unwrap();
        for language in languages.as_array().unwrap() {
            let identifier = language["identifier"].as_str().unwrap();
            config.appearance.language = identifier.into();
            let serialized = toml::to_string(&config).unwrap();
            let mut loaded: Config = toml::from_str(&serialized).unwrap();
            assert_eq!(loaded.appearance.language, identifier);
            assert!(
                loaded.validate().is_empty(),
                "{identifier}.toml: config rejected bundled identifier"
            );
        }
        let mut loaded = Config::default();
        for unsupported in ["__unsupported__", "en ", ""] {
            loaded.appearance.language = unsupported.into();
            let warnings = loaded.validate();
            assert_eq!(loaded.appearance.language, "en");
            assert_eq!(
                warnings
                    .iter()
                    .filter(|warning| warning.field == "appearance.language")
                    .count(),
                1
            );
            assert!(loaded.validate().is_empty());
        }
    }

    fn compile_rule(rule: WindowRule) -> CompiledWindowRule {
        Config {
            window_rules: vec![rule],
            ..Default::default()
        }
        .compile_window_rules()
        .remove(0)
    }

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.layout.gap, 10);
        assert_eq!(config.layout.outer_gap_left, 10);
        assert_eq!(config.layout.outer_gap_right, 10);
        assert_eq!(config.layout.outer_gap_top, 10);
        assert_eq!(config.layout.outer_gap_bottom, 10);
        assert_eq!(config.layout.width_presets, vec![0.333, 0.5, 0.667]);
        assert_eq!(config.layout.height_presets, vec![0.333, 0.5, 0.667]);
        assert_eq!(config.layout.centering_mode, CenteringModeConfig::Center);
        assert!(config.behavior.focus_new_windows);
    }

    #[test]
    fn test_config_serialization_roundtrip() {
        let mut config = Config::default();
        config.layout.gap = 23;
        config.layout.centering_mode = CenteringModeConfig::JustInView;
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.layout.gap, 23);
        assert_eq!(
            parsed.layout.centering_mode,
            CenteringModeConfig::JustInView
        );
    }

    #[test]
    fn test_config_partial_parse() {
        let toml_str = r#"
            [layout]
            gap = 20
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.layout.gap, 20);
        assert_eq!(config.layout.outer_gap_left, 10);
        assert_eq!(config.layout.width_presets, vec![0.333, 0.5, 0.667]);
    }

    #[test]
    fn fixed_size_windows_config_defaults_and_rejects_invalid_values() {
        assert_eq!(
            Config::default().behavior.fixed_size_windows,
            FixedSizeWindows::Float
        );
        for (value, expected) in [
            ("float", FixedSizeWindows::Float),
            ("tile", FixedSizeWindows::Tile),
        ] {
            let config: Config =
                toml::from_str(&format!("[behavior]\nfixed_size_windows = '{value}'")).unwrap();
            assert_eq!(config.behavior.fixed_size_windows, expected);
            let json = serde_json::to_string(&config).unwrap();
            let roundtrip: Config = serde_json::from_str(&json).unwrap();
            assert_eq!(roundtrip.behavior.fixed_size_windows, expected);
        }
        assert!(toml::from_str::<Config>("[behavior]\nfixed_size_windows = 'invalid'").is_err());
    }

    #[test]
    fn mouse_resize_config_defaults_and_rejects_invalid_values() {
        let default: Config = toml::from_str("[layout]").unwrap();
        assert_eq!(default.layout.mouse_resize, MouseResize::Presets);
        let free: Config = toml::from_str("[layout]\nmouse_resize = 'free'").unwrap();
        assert_eq!(free.layout.mouse_resize, MouseResize::Free);
        assert!(toml::from_str::<Config>("[layout]\nmouse_resize = 'invalid'").is_err());
        assert!(toml::from_str::<Config>("[layout]\ncentering_mode = 'invalid'").is_err());
    }

    #[test]
    fn offscreen_mode_config_defaults_and_rejects_invalid_values() {
        let default: Config = toml::from_str("[layout]").unwrap();
        assert_eq!(default.layout.offscreen_mode, OffscreenMode::Bleed);
        let park: Config = toml::from_str("[layout]\noffscreen_mode = 'park'").unwrap();
        assert_eq!(park.layout.offscreen_mode, OffscreenMode::Park);
        assert!(toml::from_str::<Config>("[layout]\noffscreen_mode = 'invalid'").is_err());
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
        for path in paths {
            assert_eq!(path.file_name().unwrap(), "config.toml");
        }
    }

    #[test]
    fn test_default_and_merged_hotkeys_leave_rows_unbound() {
        let defaults = HotkeyConfig::default();
        let mut saved = HotkeyConfig {
            scroll_modifier: default_scroll_modifier(),
            disabled: Vec::new(),
            bindings: HashMap::from([("Ctrl+Alt+X".to_string(), "focus_left".to_string())]),
        };
        Config::merge_default_hotkeys(&mut saved);
        let row_actions = [
            "focus_row_up",
            "focus_row_down",
            "focus_row",
            "move_window_to_row_up",
            "move_window_to_row_down",
        ];
        let mut violations = Vec::new();
        for (label, config) in [("defaults", &defaults), ("merged", &saved)] {
            for (key, action) in &config.bindings {
                if row_actions.contains(&action.as_str())
                    || ["Ctrl+Alt+PageUp", "Ctrl+Alt+PageDown"].contains(&key.as_str())
                {
                    violations.push(format!("{label}: {key} = {action}"));
                }
            }
        }
        violations.sort();
        assert!(violations.is_empty(), "{}", violations.join("; "));
    }

    #[test]
    fn test_hotkey_config_default() {
        let config = HotkeyConfig::default();
        assert_eq!(config.bindings.len(), 68);
        assert_eq!(
            config.bindings.get("Ctrl+Alt+Shift+PageUp"),
            Some(&"move_to_workspace_prev".to_string())
        );
        assert_eq!(
            config.bindings.get("Ctrl+Alt+Shift+PageDown"),
            Some(&"move_to_workspace_next".to_string())
        );
        assert_eq!(
            config.bindings.get("Ctrl+Alt+Space"),
            Some(&"toggle_overview".to_string())
        );
        assert_eq!(
            config.bindings.get("Ctrl+Alt+T"),
            Some(&"toggle_tabbed".to_string())
        );
        assert_eq!(
            config.bindings.get("Ctrl+Alt+,"),
            Some(&"consume_from_left".to_string())
        );
        assert_eq!(
            config.bindings.get("Ctrl+Alt+."),
            Some(&"consume_from_right".to_string())
        );
        assert_eq!(
            config.bindings.get("Ctrl+Alt+S"),
            Some(&"scratchpad_toggle".to_string())
        );
        assert_eq!(
            config.bindings.get("Ctrl+Alt+Shift+S"),
            Some(&"scratchpad_stash".to_string())
        );
        assert_eq!(
            config.bindings.get("Ctrl+Alt+Y"),
            Some(&"toggle_sticky".to_string())
        );
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
            config.bindings.get("Ctrl+Alt+-"),
            Some(&"cycle_width_down".to_string())
        );
        assert_eq!(
            config.bindings.get("Ctrl+Alt+Win+,"),
            Some(&"focus_monitor_left".to_string())
        );
        assert_eq!(
            config.bindings.get("Win+Ctrl+Escape"),
            Some(&"panic_revert".to_string())
        );
    }

    #[test]
    fn test_parse_wrap_cycle_commands() {
        use leopardwm_ipc::IpcCommand;
        for (name, expected) in [
            ("cycle_width", IpcCommand::CycleWidth),
            ("CYCLE-WIDTH", IpcCommand::CycleWidth),
            ("cycle_height", IpcCommand::CycleHeight),
            ("CYCLE-HEIGHT", IpcCommand::CycleHeight),
        ] {
            assert_eq!(parse_command(name), Some(expected), "{name}");
        }
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
        assert_eq!(parse_command("resize_grow"), Some(IpcCommand::CycleWidthUp));
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
        let mut user = HotkeyConfig {
            scroll_modifier: default_scroll_modifier(),
            disabled: Vec::new(),
            bindings: HashMap::new(),
        };
        user.bindings
            .insert("Ctrl+Alt+X".to_string(), "focus_left".to_string());

        Config::merge_default_hotkeys(&mut user);

        assert_eq!(
            user.bindings.get("Ctrl+Alt+X"),
            Some(&"focus_left".to_string())
        );
        assert!(!user.bindings.contains_key("Ctrl+Alt+H"));

        assert_eq!(
            user.bindings.get("Ctrl+Alt+["),
            Some(&"move_window_left".to_string())
        );
        assert_eq!(
            user.bindings.get("Ctrl+Alt+Shift+J"),
            Some(&"move_window_down".to_string())
        );
    }

    #[test]
    fn test_hotkey_merge_preserves_user_binding_on_default_key() {
        let mut user = HotkeyConfig {
            scroll_modifier: default_scroll_modifier(),
            disabled: Vec::new(),
            bindings: HashMap::from([("Ctrl+Alt+H".to_string(), "focus_right".to_string())]),
        };

        Config::merge_default_hotkeys(&mut user);

        assert_eq!(
            user.bindings.get("Ctrl+Alt+H").map(String::as_str),
            Some("focus_right")
        );
        assert!(!user.bindings.values().any(|cmd| cmd == "focus_left"));
    }

    #[test]
    fn test_hotkey_merge_preserves_user_binding_with_alternate_spelling() {
        let mut user = HotkeyConfig {
            scroll_modifier: default_scroll_modifier(),
            disabled: Vec::new(),
            bindings: HashMap::from([("alt+ctrl+h".to_string(), "focus_right".to_string())]),
        };

        Config::merge_default_hotkeys(&mut user);

        assert_eq!(
            user.bindings.get("alt+ctrl+h").map(String::as_str),
            Some("focus_right")
        );
        assert!(!user.bindings.contains_key("Ctrl+Alt+H"));
    }

    #[test]
    fn test_merge_skips_disabled_commands() {
        let mut hk = HotkeyConfig {
            scroll_modifier: default_scroll_modifier(),
            disabled: vec!["consume_from_right".to_string()],
            bindings: HashMap::new(),
        };
        Config::merge_default_hotkeys(&mut hk);

        assert!(
            !hk.bindings.values().any(|c| c == "consume_from_right"),
            "disabled command should stay unbound after merge"
        );
        assert!(hk.bindings.values().any(|c| c == "focus_left"));
    }

    #[test]
    fn test_merge_does_not_overwrite_user_bound_default_key() {
        let mut hk = HotkeyConfig {
            scroll_modifier: default_scroll_modifier(),
            disabled: Vec::new(),
            bindings: HashMap::new(),
        };
        // User bound Ctrl+Alt+Shift+PageUp (the default for move_to_workspace_prev) to move_window_to_row_up.
        hk.bindings.insert(
            "Ctrl+Alt+Shift+PageUp".to_string(),
            "move_window_to_row_up".to_string(),
        );
        Config::merge_default_hotkeys(&mut hk);

        assert_eq!(
            hk.bindings.get("Ctrl+Alt+Shift+PageUp").map(String::as_str),
            Some("move_window_to_row_up"),
            "merge_default_hotkeys must not overwrite a key the user already bound"
        );
    }

    #[test]
    fn test_disabled_deserializes_from_json_save_payload() {
        // Mirrors the settings-save path (JS object -> serde_json -> Config).
        let json = serde_json::json!({
            "scroll_modifier": "Ctrl+Alt",
            "disabled": ["consume_from_right"],
            "Ctrl+Alt+H": "focus_left",
        });
        let hk: HotkeyConfig = serde_json::from_value(json).unwrap();
        assert_eq!(hk.disabled, vec!["consume_from_right".to_string()]);
        assert_eq!(
            hk.bindings.get("Ctrl+Alt+H"),
            Some(&"focus_left".to_string())
        );
        // `disabled` must not leak into the flattened bindings map.
        assert!(!hk.bindings.contains_key("disabled"));
    }

    #[test]
    fn test_disabled_roundtrips_with_flattened_bindings() {
        let mut hk = HotkeyConfig {
            disabled: vec!["consume_from_right".to_string()],
            ..HotkeyConfig::default()
        };
        hk.bindings.clear();
        hk.bindings
            .insert("Ctrl+Alt+H".to_string(), "focus_left".to_string());
        let toml_str = toml::to_string_pretty(&hk).unwrap();
        let parsed: HotkeyConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.disabled, vec!["consume_from_right".to_string()]);
        assert_eq!(
            parsed.bindings.get("Ctrl+Alt+H"),
            Some(&"focus_left".to_string())
        );
        assert_eq!(parsed.scroll_modifier, hk.scroll_modifier);
    }

    #[test]
    fn test_default_column_width_px() {
        let config = LayoutConfig::default();
        assert_eq!(config.default_column_width_px(1920), 626);
        assert_eq!(config.default_column_width_px(1922), 626);
    }

    #[test]
    fn test_default_width_preset_selects_fraction() {
        let mut config = LayoutConfig::default();
        assert_eq!(config.default_width_fraction(), 0.333);

        config.default_width_preset = 2;
        assert_eq!(config.default_width_fraction(), 0.5);
        config.default_width_preset = 3;
        assert_eq!(config.default_width_fraction(), 0.667);

        config.default_width_preset = 0;
        assert_eq!(config.default_width_fraction(), 0.333);
        config.default_width_preset = 99;
        assert_eq!(config.default_width_fraction(), 0.333);
    }

    #[test]
    fn test_default_width_preset_out_of_range_warns_and_clamps() {
        let mut config = Config::default();
        config.layout.default_width_preset = 5;
        let warnings = config.validate();
        assert!(warnings
            .iter()
            .any(|w| w.field == "layout.default_width_preset"));
        assert_eq!(config.layout.default_width_preset, 1);
    }

    #[test]
    fn border_rule_config_round_trip_and_validation() {
        let source = r##"
            [[window_rules]]
            match_class = "One"
            border = false
            border_color = "a1B2c3"
            [[window_rules]]
            match_class = "Two"
            [[window_rules]]
            match_class = "Three"
            border_color = "#12345"
        "##;
        let mut config: Config = toml::from_str(source).unwrap();
        assert_eq!(config.window_rules[0].border, Some(false));
        assert_eq!(
            config.window_rules[0].border_color.as_deref(),
            Some("a1B2c3")
        );
        assert_eq!(config.window_rules[1].border, None);
        assert_eq!(config.window_rules[1].border_color, None);
        let round_trip: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(round_trip.window_rules[0].border, Some(false));
        assert_eq!(
            round_trip.window_rules[0].border_color.as_deref(),
            Some("a1B2c3")
        );
        assert_eq!(round_trip.window_rules[1].border, None);
        assert_eq!(round_trip.window_rules[1].border_color, None);
        let warnings = config.validate();
        assert!(warnings
            .iter()
            .any(|w| w.field == "window_rules[2].border_color"));
        assert_eq!(config.window_rules[2].border_color, None);
        assert_eq!(
            config.window_rules[0].border_color.as_deref(),
            Some("a1B2c3")
        );
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
            corner_style: None,
            border: None,
            border_color: None,
            open_on_workspace: None,
            open_maximized: false,
            column_width: None,
            open_in_column: None,
            row: None,
            sticky: false,
            match_max_width: None,
            match_max_height: None,
            ..Default::default()
        };

        let rule = compile_rule(rule);
        assert!(rule.matches("Notepad", "Untitled - Notepad", "notepad.exe", None));
        assert!(rule.matches("Notepad", "Any Title", "any.exe", None));
        assert!(rule.matches("Notepad", "Different Title", "different.exe", None));
        assert!(!rule.matches("Chrome_WidgetWin_1", "Google Chrome", "chrome.exe", None));
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
            corner_style: None,
            border: None,
            border_color: None,
            open_on_workspace: None,
            open_maximized: false,
            column_width: None,
            open_in_column: None,
            row: None,
            sticky: false,
            match_max_width: None,
            match_max_height: None,
            ..Default::default()
        };

        let rule = compile_rule(rule);
        assert!(rule.matches(
            "Chrome_WidgetWin_1",
            "DevTools - localhost:3000",
            "chrome.exe",
            None
        ));
        assert!(rule.matches("SomeClass", "Firefox DevTools", "firefox.exe", None));
        assert!(rule.matches("DifferentClass", "Firefox DevTools", "different.exe", None));
        assert!(!rule.matches("Chrome_WidgetWin_1", "Google Chrome", "chrome.exe", None));
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
    fn floating_rule_position_survives_config_and_settings_roundtrip() {
        let config: Config = toml::from_str("[[window_rules]]\nmatch_class = 'App'\naction = 'float'\nx = -25\ny = 0\n\n[[window_rules]]\nmatch_class = 'Other'\naction = 'float'\n").unwrap();
        let settings = serde_json::to_value(&config).unwrap();
        assert_eq!(settings["window_rules"][0]["x"], -25);
        assert_eq!(settings["window_rules"][0]["y"], 0);
        assert!(settings["window_rules"][1]["x"].is_null());
        assert!(settings["window_rules"][1]["y"].is_null());
        let saved: Config = serde_json::from_value(settings).unwrap();
        let restored: Config = toml::from_str(&toml::to_string(&saved).unwrap()).unwrap();
        let result = serde_json::to_value(restored).unwrap();
        assert_eq!(result["window_rules"][0]["x"], -25);
        assert_eq!(result["window_rules"][0]["y"], 0);
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
    fn move_step_defaults_roundtrips_and_clamps() {
        let missing: Config = toml::from_str("[behavior]\n").unwrap();
        assert_eq!(missing.behavior.move_step, 50);
        let explicit: Config = toml::from_str("[behavior]\nmove_step = 125\n").unwrap();
        let roundtrip: Config = toml::from_str(&toml::to_string(&explicit).unwrap()).unwrap();
        assert_eq!(roundtrip.behavior.move_step, 125);
        for (input, expected) in [(-1, 1), (0, 1), (1001, 1000)] {
            let mut config: Config =
                toml::from_str(&format!("[behavior]\nmove_step = {input}\n")).unwrap();
            assert!(config
                .validate()
                .iter()
                .any(|w| w.field == "behavior.move_step"));
            assert_eq!(config.behavior.move_step, expected);
        }
    }

    #[test]
    fn keep_awake_defaults_off_and_round_trips() {
        let missing: Config = toml::from_str("[behavior]").unwrap();
        assert!(!missing.behavior.keep_awake);
        assert!(!Config::default().behavior.keep_awake);
        let enabled: Config = toml::from_str("[behavior]\nkeep_awake = true").unwrap();
        assert!(enabled.behavior.keep_awake);
        let roundtrip: Config = toml::from_str(&toml::to_string(&enabled).unwrap()).unwrap();
        assert!(roundtrip.behavior.keep_awake);
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

    #[test]
    fn test_window_rule_multiple_matches_uses_first() {
        let rules = vec![
            WindowRule {
                match_class: Some("Notepad".to_string()),
                match_title: None,
                match_executable: None,
                action: WindowAction::Float,
                width: Some(800),
                height: Some(600),
                corner_style: None,
                border: None,
                border_color: None,
                open_on_workspace: None,
                open_maximized: false,
                column_width: None,
                open_in_column: None,
                row: None,
                sticky: false,
                match_max_width: None,
                match_max_height: None,
                ..Default::default()
            },
            WindowRule {
                match_class: Some("Notepad".to_string()),
                match_title: None,
                match_executable: None,
                action: WindowAction::Ignore,
                width: None,
                height: None,
                corner_style: None,
                border: None,
                border_color: None,
                open_on_workspace: None,
                open_maximized: false,
                column_width: None,
                open_in_column: None,
                row: None,
                sticky: false,
                match_max_width: None,
                match_max_height: None,
                ..Default::default()
            },
        ];

        let config = Config {
            window_rules: rules,
            ..Default::default()
        };
        let state = crate::AppState::new_with_config(config, vec![]);
        assert_eq!(
            state.evaluate_window_rules("Notepad", "Untitled", "notepad.exe", None),
            WindowAction::Float
        );
    }

    #[test]
    fn test_window_rule_regex_special_chars() {
        let rule = WindowRule {
            match_class: None,
            match_title: Some(r"^\[DEBUG\].*$".to_string()),
            match_executable: None,
            action: WindowAction::Ignore,
            width: None,
            height: None,
            corner_style: None,
            border: None,
            border_color: None,
            open_on_workspace: None,
            open_maximized: false,
            column_width: None,
            open_in_column: None,
            row: None,
            sticky: false,
            match_max_width: None,
            match_max_height: None,
            ..Default::default()
        };

        let rule = compile_rule(rule);
        assert!(rule.matches("AnyClass", "[DEBUG] Application started", "app.exe", None));
        assert!(!rule.matches("AnyClass", "DEBUG Application started", "app.exe", None));
    }

    #[test]
    fn test_window_rule_regex_case_sensitivity() {
        let rule = WindowRule {
            match_class: None,
            match_title: Some("Error".to_string()),
            match_executable: None,
            action: WindowAction::Float,
            width: None,
            height: None,
            corner_style: None,
            border: None,
            border_color: None,
            open_on_workspace: None,
            open_maximized: false,
            column_width: None,
            open_in_column: None,
            row: None,
            sticky: false,
            match_max_width: None,
            match_max_height: None,
            ..Default::default()
        };

        let rule = compile_rule(rule);
        assert!(rule.matches("AnyClass", "Error Dialog", "app.exe", None));
        assert!(!rule.matches("AnyClass", "error dialog", "app.exe", None));
    }

    #[test]
    fn test_window_rule_regex_case_insensitive() {
        let rule = WindowRule {
            match_class: None,
            match_title: Some("(?i)error".to_string()),
            match_executable: None,
            action: WindowAction::Float,
            width: None,
            height: None,
            corner_style: None,
            border: None,
            border_color: None,
            open_on_workspace: None,
            open_maximized: false,
            column_width: None,
            open_in_column: None,
            row: None,
            sticky: false,
            match_max_width: None,
            match_max_height: None,
            ..Default::default()
        };

        let rule = compile_rule(rule);
        assert!(rule.matches("AnyClass", "Error Dialog", "app.exe", None));
        assert!(rule.matches("AnyClass", "error dialog", "app.exe", None));
        assert!(rule.matches("AnyClass", "ERROR DIALOG", "app.exe", None));
    }

    #[test]
    fn test_window_rule_invalid_regex_returns_false() {
        let rule = WindowRule {
            match_class: None,
            match_title: Some("[invalid(regex".to_string()),
            match_executable: None,
            action: WindowAction::Float,
            width: None,
            height: None,
            corner_style: None,
            border: None,
            border_color: None,
            open_on_workspace: None,
            open_maximized: false,
            column_width: None,
            open_in_column: None,
            row: None,
            sticky: false,
            match_max_width: None,
            match_max_height: None,
            ..Default::default()
        };

        let config = Config {
            window_rules: vec![rule],
            ..Default::default()
        };
        let compiled = config.compile_window_rules();
        assert_eq!(
            compiled.len(),
            BUILTIN_IGNORE_EXECUTABLES.len() + BUILTIN_FLOAT_RULES.len()
        );
        assert!(!compiled.iter().any(|rule| rule.matches(
            "AnyClass",
            "Any Title",
            "any.exe",
            None
        )));
    }

    #[test]
    fn test_window_rule_empty_strings_match() {
        let rule = WindowRule {
            match_class: Some(".*".to_string()),
            match_title: None,
            match_executable: None,
            action: WindowAction::Float,
            width: None,
            height: None,
            corner_style: None,
            border: None,
            border_color: None,
            open_on_workspace: None,
            open_maximized: false,
            column_width: None,
            open_in_column: None,
            row: None,
            sticky: false,
            match_max_width: None,
            match_max_height: None,
            ..Default::default()
        };

        let rule = compile_rule(rule);
        assert!(rule.matches("", "Title", "app.exe", None));
        assert!(rule.matches("SomeClass", "Title", "app.exe", None));
    }

    #[test]
    fn test_window_rule_width_height_optional() {
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

    #[test]
    fn test_window_rule_column_width_deserializes_settings_payload() {
        let with_width: WindowRule = serde_json::from_value(serde_json::json!({
            "match_executable": "editor.exe",
            "column_width": 0.5,
        }))
        .unwrap();
        assert_eq!(with_width.column_width, Some(0.5));

        let cleared: WindowRule = serde_json::from_value(serde_json::json!({
            "match_executable": "editor.exe",
        }))
        .unwrap();
        assert_eq!(cleared.column_width, None);
    }

    #[test]
    fn test_window_rule_column_width_compilation_rejects_invalid_values() {
        fn compiled_width(width: f64) -> Option<f64> {
            let config = Config {
                window_rules: vec![WindowRule {
                    column_width: Some(width),
                    ..WindowRule::default()
                }],
                ..Config::default()
            };
            config.compile_window_rules()[0].column_width
        }

        assert_eq!(compiled_width(0.05), Some(0.05));
        assert_eq!(compiled_width(1.0), Some(1.0));
        for invalid in [0.049, 1.001, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(compiled_width(invalid), None);
        }
    }

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
        assert!(warnings.iter().any(|w| w.field == "layout.outer_gap_left"));
    }

    #[test]
    fn test_validate_empty_width_presets_resets() {
        let mut config = Config::default();
        config.layout.width_presets = vec![];
        let warnings = config.validate();
        assert_eq!(config.layout.width_presets, vec![0.333, 0.5, 0.667]);
        assert!(warnings.iter().any(|w| w.field == "layout.width_presets"));
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
    fn test_animation_config_defaults() {
        let config = Config::default();
        assert_eq!(config.animation.layout_duration_ms, 150);
        assert_eq!(config.animation.workspace_switch_duration_ms, 200);
        assert_eq!(config.animation.scroll_duration_ms, 200);
        assert_eq!(config.animation.overview_duration_ms, 150);
        assert_eq!(
            config.animation.easing,
            leopardwm_core_layout::Easing::EaseOut
        );
        assert!(config.animation.reduce_motion_on_battery);
    }

    #[test]
    fn test_reduce_motion_on_battery_parses_from_toml() {
        let toml = "[animation]\nreduce_motion_on_battery = false\n";
        let config: Config = toml::from_str(toml).expect("parse");
        assert!(!config.animation.reduce_motion_on_battery);
        let config: Config = toml::from_str("[animation]\n").expect("parse");
        assert!(config.animation.reduce_motion_on_battery);
    }

    #[test]
    fn test_reduce_motion_on_battery_survives_settings_json_round_trip() {
        let mut config = Config::default();
        config.animation.reduce_motion_on_battery = false;

        let saved: Config =
            serde_json::from_value(serde_json::to_value(config).expect("serialize"))
                .expect("deserialize");

        assert!(!saved.animation.reduce_motion_on_battery);
    }

    #[test]
    fn test_diagnostic_capture_secs_defaults_off_and_clamps() {
        assert_eq!(Config::default().gestures.diagnostic_capture_secs, 0);
        assert_eq!(clamp_diagnostic_capture_secs(0), 0);
        assert_eq!(clamp_diagnostic_capture_secs(15), 15);
        assert_eq!(clamp_diagnostic_capture_secs(120), 120);
        assert_eq!(
            clamp_diagnostic_capture_secs(121),
            MAX_DIAGNOSTIC_CAPTURE_SECS
        );
        assert_eq!(
            clamp_diagnostic_capture_secs(9_999),
            MAX_DIAGNOSTIC_CAPTURE_SECS
        );

        let config: Config = toml::from_str("[gestures]\nenabled = true\n").expect("parse");
        assert_eq!(config.gestures.diagnostic_capture_secs, 0);

        let config: Config =
            toml::from_str("[gestures]\ndiagnostic_capture_secs = 15\n").expect("parse");
        assert_eq!(config.gestures.diagnostic_capture_secs, 15);
        assert!(config.gestures.enabled);

        let mut config = Config::default();
        config.gestures.diagnostic_capture_secs = 500;
        let warnings = config.validate();
        assert_eq!(
            config.gestures.diagnostic_capture_secs,
            MAX_DIAGNOSTIC_CAPTURE_SECS
        );
        assert!(warnings
            .iter()
            .any(|w| w.field == "gestures.diagnostic_capture_secs"));
    }

    #[test]
    fn wheel_swipes_setting_defaults_on_and_round_trips() {
        for source in ["", "[gestures]\nenabled = true\n"] {
            let config: Config = toml::from_str(source).unwrap();
            assert_eq!(
                serde_json::to_value(&config).unwrap()["gestures"]["wheel_swipes"],
                true
            );
        }
        assert_eq!(
            serde_json::to_value(Config::default()).unwrap()["gestures"]["wheel_swipes"],
            true
        );
        for enabled in [false, true] {
            let config: Config =
                toml::from_str(&format!("[gestures]\nwheel_swipes = {enabled}\n")).unwrap();
            let saved = toml::to_string(&config).unwrap();
            let restored: Config = toml::from_str(&saved).unwrap();
            let json = serde_json::to_value(&restored).unwrap();
            assert_eq!(json["gestures"]["wheel_swipes"], enabled);
            let from_settings: Config = serde_json::from_value(json).unwrap();
            assert_eq!(
                serde_json::to_value(from_settings).unwrap()["gestures"]["wheel_swipes"],
                enabled
            );
        }
    }

    #[test]
    fn raw_touchpad_setting_is_opt_in_and_round_trips() {
        let default = Config::default();
        assert!(!default.gestures.raw_input);
        let config: Config = toml::from_str("[gestures]\nraw_input = true\n").unwrap();
        assert!(config.gestures.raw_input);
        let saved = toml::to_string(&config).unwrap();
        assert!(saved.contains("raw_input = true"));
        assert!(toml::from_str::<Config>(&saved).unwrap().gestures.raw_input);
    }

    #[test]
    fn test_diagnostic_capture_secs_json_round_trip_preserves_nonzero() {
        let mut config = Config::default();
        config.gestures.diagnostic_capture_secs = 15;
        let saved: Config =
            serde_json::from_value(serde_json::to_value(config).expect("serialize"))
                .expect("deserialize");
        assert_eq!(saved.gestures.diagnostic_capture_secs, 15);

        let default_saved: Config =
            serde_json::from_value(serde_json::to_value(Config::default()).expect("serialize"))
                .expect("deserialize");
        assert_eq!(default_saved.gestures.diagnostic_capture_secs, 0);
        let json = serde_json::to_value(Config::default()).expect("serialize");
        assert!(json["gestures"].get("diagnostic_capture_secs").is_none());
    }

    #[test]
    fn test_validate_animation_duration_clamped() {
        let mut config = Config::default();
        config.animation.layout_duration_ms = 99_999;
        let warnings = config.validate();
        assert_eq!(
            config.animation.layout_duration_ms,
            MAX_ANIMATION_DURATION_MS
        );
        assert!(warnings
            .iter()
            .any(|w| w.field == "animation.layout_duration_ms"));
    }

    #[test]
    fn test_animation_easing_parses_from_toml() {
        let toml = "[animation]\neasing = \"ease_in_out\"\nlayout_duration_ms = 80\n";
        let config: Config = toml::from_str(toml).expect("parse");
        assert_eq!(
            config.animation.easing,
            leopardwm_core_layout::Easing::EaseInOut
        );
        assert_eq!(config.animation.layout_duration_ms, 80);
        assert_eq!(config.animation.scroll_duration_ms, 200);
        assert_eq!(config.animation.overview_duration_ms, 150);
    }

    #[test]
    fn test_overview_duration_parses_and_clamps() {
        let toml = "[animation]\noverview_duration_ms = 90\n";
        let config: Config = toml::from_str(toml).expect("parse");
        assert_eq!(config.animation.overview_duration_ms, 90);

        let mut config = Config::default();
        config.animation.overview_duration_ms = 99_999;
        let warnings = config.validate();
        assert_eq!(
            config.animation.overview_duration_ms,
            MAX_ANIMATION_DURATION_MS
        );
        assert!(warnings
            .iter()
            .any(|w| w.field == "animation.overview_duration_ms"));
    }

    #[test]
    fn test_workspace_names_resolve_by_position() {
        let mut config = Config::default();
        config.workspaces.names = vec!["web".to_string(), "".to_string(), "  chat  ".to_string()];
        assert_eq!(config.workspaces.name_for(0).as_deref(), Some("web"));
        assert_eq!(config.workspaces.name_for(1), None);
        assert_eq!(config.workspaces.name_for(2).as_deref(), Some("chat"));
        assert_eq!(config.workspaces.name_for(8), None);
    }

    #[test]
    fn test_workspace_names_parse_from_toml() {
        let toml = "[workspaces]\nnames = [\"web\", \"code\", \"chat\"]\n";
        let config: Config = toml::from_str(toml).expect("parse");
        assert_eq!(config.workspaces.name_for(0).as_deref(), Some("web"));
        assert_eq!(config.workspaces.name_for(2).as_deref(), Some("chat"));
    }

    #[test]
    fn test_overview_render_defaults_to_live() {
        let config: Config = toml::from_str("").expect("parse");
        assert_eq!(config.overview.render, OverviewRender::Live);
        assert_eq!(Config::default().overview.render, OverviewRender::Live);
    }

    #[test]
    fn test_overview_render_parses_snapshot() {
        let toml = "[overview]\nrender = \"snapshot\"\n";
        let config: Config = toml::from_str(toml).expect("parse");
        assert_eq!(config.overview.render, OverviewRender::Snapshot);
    }

    #[test]
    fn test_overview_render_parses_placeholder() {
        let toml = "[overview]\nrender = \"placeholder\"\n";
        let config: Config = toml::from_str(toml).expect("parse");
        assert_eq!(config.overview.render, OverviewRender::Placeholder);

        let toml = "[overview]\nrender = \"live\"\n";
        let config: Config = toml::from_str(toml).expect("parse");
        assert_eq!(config.overview.render, OverviewRender::Live);
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
                    corner_style: None,
                    border: None,
                    border_color: None,
                    open_on_workspace: None,
                    open_maximized: false,
                    column_width: None,
                    open_in_column: None,
                    row: None,
                    sticky: false,
                    match_max_width: None,
                    match_max_height: None,
                    ..Default::default()
                },
                WindowRule {
                    match_class: None,
                    match_title: None,
                    match_executable: Some("notepad.exe".to_string()),
                    action: WindowAction::Tile,
                    width: None,
                    height: None,
                    corner_style: None,
                    border: None,
                    border_color: None,
                    open_on_workspace: None,
                    open_maximized: false,
                    column_width: None,
                    open_in_column: None,
                    row: None,
                    sticky: false,
                    match_max_width: None,
                    match_max_height: None,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let compiled = config.compile_window_rules();
        assert_eq!(
            compiled.len(),
            2 + BUILTIN_IGNORE_EXECUTABLES.len() + BUILTIN_FLOAT_RULES.len()
        );

        assert!(compiled[0].matches(
            "Chrome_WidgetWin_1",
            "YouTube - Google Chrome",
            "chrome.exe",
            None
        ));
        assert!(!compiled[0].matches("Firefox", "YouTube", "firefox.exe", None));
        assert!(!compiled[0].matches("Chrome_WidgetWin_1", "Google Chrome", "chrome.exe", None));

        assert!(compiled[1].matches("AnyClass", "Any Title", "notepad.exe", None));
        assert!(compiled[1].matches("AnyClass", "Any Title", "NOTEPAD.EXE", None));
        assert!(!compiled[1].matches("AnyClass", "Any Title", "wordpad.exe", None));
    }

    #[test]
    fn audacity_import_float_rule_and_user_override() {
        let defaults = Config::default().compile_window_rules();
        let override_rules = Config {
            window_rules: vec![WindowRule {
                match_executable: Some("Audacity.exe".into()),
                match_title: Some("^Importing".into()),
                action: WindowAction::Tile,
                ..Default::default()
            }],
            ..Default::default()
        }
        .compile_window_rules();

        for (rules, title, executable, expected) in [
            (
                &defaults,
                "Importing WAV (Microsoft)",
                "Audacity.exe",
                Some(WindowAction::Float),
            ),
            (&defaults, "Audacity", "Audacity.exe", None),
            (&defaults, "Importing WAV (Microsoft)", "other.exe", None),
            (
                &override_rules,
                "Importing WAV (Microsoft)",
                "Audacity.exe",
                Some(WindowAction::Tile),
            ),
        ] {
            let action = rules
                .iter()
                .find(|rule| rule.matches("AnyClass", title, executable, None))
                .map(|rule| rule.action);
            assert_eq!(action, expected, "title={title}, executable={executable}");
        }
    }

    #[test]
    fn invalid_rule_warnings_do_not_log_private_patterns() {
        use std::io::{self, Write};
        use std::sync::{Arc, Mutex};

        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().write(bytes)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || Capture(writer.clone()))
            .finish();
        let config = Config {
            window_rules: vec![
                WindowRule {
                    match_class: Some("private-class(".into()),
                    ..Default::default()
                },
                WindowRule {
                    match_title: Some("private-title(".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        tracing::subscriber::with_default(subscriber, || config.compile_window_rules());
        let text = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(!text.contains("private-class"), "{text}");
        assert!(!text.contains("private-title"), "{text}");
        assert!(!text.contains("regex parse error"), "{text}");
        assert!(text.contains("Window rule 1: invalid regex"), "{text}");
        assert!(text.contains("Window rule 2: invalid regex"), "{text}");
    }

    #[test]
    fn test_compiled_window_rule_invalid_regex_skipped() {
        let config = Config {
            window_rules: vec![
                WindowRule {
                    match_class: Some("[invalid(regex".to_string()),
                    match_title: None,
                    match_executable: None,
                    action: WindowAction::Float,
                    width: None,
                    height: None,
                    corner_style: None,
                    border: None,
                    border_color: None,
                    open_on_workspace: None,
                    open_maximized: false,
                    column_width: None,
                    open_in_column: None,
                    row: None,
                    sticky: false,
                    match_max_width: None,
                    match_max_height: None,
                    ..Default::default()
                },
                WindowRule {
                    match_class: Some("ValidClass".to_string()),
                    match_title: None,
                    match_executable: None,
                    action: WindowAction::Tile,
                    width: None,
                    height: None,
                    corner_style: None,
                    border: None,
                    border_color: None,
                    open_on_workspace: None,
                    open_maximized: false,
                    column_width: None,
                    open_in_column: None,
                    row: None,
                    sticky: false,
                    match_max_width: None,
                    match_max_height: None,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let compiled = config.compile_window_rules();
        assert_eq!(
            compiled.len(),
            1 + BUILTIN_IGNORE_EXECUTABLES.len() + BUILTIN_FLOAT_RULES.len()
        );
        assert!(compiled[0].matches("ValidClass", "Any Title", "any.exe", None));
    }

    #[test]
    fn test_window_rule_size_matching() {
        for (max_width, max_height, size, class, expected) in [
            (Some(400), Some(300), Some((400, 300)), "QQ", true),
            (Some(400), Some(300), Some((401, 300)), "QQ", false),
            (Some(400), Some(300), Some((400, 301)), "QQ", false),
            (Some(400), Some(300), None, "QQ", false),
            (Some(400), None, Some((400, 900)), "QQ", true),
            (None, Some(300), Some((900, 300)), "QQ", true),
            (Some(400), Some(300), Some((400, 300)), "Other", false),
            (None, None, None, "QQ", true),
        ] {
            let rule = WindowRule {
                match_class: Some("^QQ$".into()),
                match_title: Some("^QQ NT$".into()),
                match_executable: Some("QQ.exe".into()),
                match_max_width: max_width,
                match_max_height: max_height,
                action: WindowAction::Ignore,
                ..Default::default()
            };
            let config = Config {
                window_rules: vec![rule],
                ..Default::default()
            };
            assert_eq!(
                config.compile_window_rules()[0].matches(class, "QQ NT", "qq.EXE", size),
                expected
            );
        }
        let rule = compile_rule(WindowRule::default());
        assert!(!rule.matches("AnyClass", "Any Title", "any.exe", None));
        let rule = compile_rule(WindowRule {
            match_max_width: Some(400),
            match_max_height: Some(300),
            ..Default::default()
        });
        assert!(!rule.matches("QQ", "QQ NT", "QQ.exe", Some((100, 100))));
    }

    #[test]
    fn test_window_rule_size_limits_validate_and_roundtrip() {
        let config: Config = toml::from_str(
            r#"
            [[window_rules]]
            match_executable = "QQ.exe"
            match_max_width = 400
            match_max_height = 300
            action = "ignore"
            [[window_rules]]
            match_executable = "QQ.exe"
            match_max_width = 0
            match_max_height = -1
            action = "float"
        "#,
        )
        .unwrap();
        let saved = toml::to_string(&config).unwrap();
        let restored: Config = toml::from_str(&saved).unwrap();
        let compiled = restored.compile_window_rules();
        assert_eq!(compiled[0].match_max_width, Some(400));
        assert_eq!(compiled[0].match_max_height, Some(300));
        assert_eq!(compiled[1].match_max_width, None);
        assert_eq!(compiled[1].match_max_height, None);
        assert_eq!(compiled[1].action, WindowAction::Float);
        assert!(compiled[1].matches("QQ", "QQ NT", "QQ.exe", None));
        for rule in Config::default().compile_window_rules() {
            assert!(
                !rule.has_size_condition(),
                "defaults must not add size thresholds"
            );
        }
    }

    #[test]
    fn test_compiled_window_rule_slot_and_sticky() {
        let config = Config {
            window_rules: vec![
                WindowRule {
                    match_class: Some("A".to_string()),
                    open_in_column: Some(3),
                    sticky: true,
                    ..Default::default()
                },
                WindowRule {
                    match_class: Some("B".to_string()),
                    open_in_column: Some(0), // 0 violates 1-based, dropped
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let compiled = config.compile_window_rules();
        assert_eq!(compiled[0].open_in_column, Some(2)); // 1-based 3 -> 0-based 2
        assert!(compiled[0].sticky);
        assert_eq!(compiled[1].open_in_column, None);
        assert!(!compiled[1].sticky);
    }

    #[test]
    fn test_new_window_monitor_defaults_and_roundtrips() {
        assert_eq!(
            Config::default().behavior.new_window_monitor,
            NewWindowMonitor::Opening
        );
        for text in ["", "[behavior]\n"] {
            let config: Config = toml::from_str(text).unwrap();
            assert_eq!(
                config.behavior.new_window_monitor,
                NewWindowMonitor::Opening
            );
        }
        for (value, expected) in [
            ("opening", NewWindowMonitor::Opening),
            ("focused", NewWindowMonitor::Focused),
        ] {
            let text = format!("[behavior]\nnew_window_monitor = \"{value}\"\n");
            let config: Config = toml::from_str(&text).unwrap();
            assert_eq!(config.behavior.new_window_monitor, expected);
            let encoded = toml::to_string_pretty(&config).unwrap();
            assert!(encoded.contains(&format!("new_window_monitor = \"{value}\"")));
            let decoded: Config = toml::from_str(&encoded).unwrap();
            assert_eq!(decoded.behavior.new_window_monitor, expected);
            let json = serde_json::to_string(&config).unwrap();
            let decoded: Config = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded.behavior.new_window_monitor, expected);
        }
        assert!(
            toml::from_str::<Config>("[behavior]\nnew_window_monitor = \"primary\"\n").is_err()
        );
    }

    #[test]
    fn test_rows_config_parse_shares() {
        let rows = RowsConfig {
            split: vec!["100%".to_string()],
            gap: 8,
            ..Default::default()
        };
        assert_eq!(rows.parse_shares(), vec![1.0]);

        let rows = RowsConfig {
            split: vec!["60%".to_string(), "40%".to_string()],
            gap: 8,
            ..Default::default()
        };
        let shares = rows.parse_shares();
        assert_eq!(shares.len(), 2);
        assert!((shares[0] - 0.6).abs() < 1e-6);
        assert!((shares[1] - 0.4).abs() < 1e-6);

        // Normalized when sum != 100%
        let rows = RowsConfig {
            split: vec!["50%".to_string(), "30%".to_string()],
            gap: 8,
            ..Default::default()
        };
        let shares = rows.parse_shares();
        assert_eq!(shares.len(), 2);
        assert!((shares[0] - 0.625).abs() < 1e-6);
        assert!((shares[1] - 0.375).abs() < 1e-6);

        // Clamped to at most 4 rows
        let rows = RowsConfig {
            split: vec![
                "20%".to_string(),
                "20%".to_string(),
                "20%".to_string(),
                "20%".to_string(),
                "20%".to_string(),
            ],
            gap: 8,
            ..Default::default()
        };
        let shares = rows.parse_shares();
        assert_eq!(shares.len(), 4);
        for s in shares {
            assert!((s - 0.25).abs() < 1e-6);
        }

        // Invalid and zero entries ignored; fallback to 1.0 if empty
        let rows = RowsConfig {
            split: vec!["invalid".to_string(), "0%".to_string()],
            gap: 8,
            ..Default::default()
        };
        assert_eq!(rows.parse_shares(), vec![1.0]);

        let rows = RowsConfig {
            split: vec![],
            gap: 8,
            ..Default::default()
        };
        assert_eq!(rows.parse_shares(), vec![1.0]);
    }

    #[test]
    fn test_per_workspace_rows_config_parse_shares() {
        let rows = RowsConfig {
            workspaces: vec![
                WorkspaceRowsConfig {
                    workspaces: vec![1, 2],
                    split: vec!["50%".to_string(), "50%".to_string()],
                },
                WorkspaceRowsConfig {
                    workspaces: vec![3],
                    split: vec!["1/3".to_string(), "1/3".to_string(), "1/3".to_string()],
                },
            ],
            gap: 8,
            ..Default::default()
        };

        // Workspace 1 (index 0) and Workspace 2 (index 1) get 2 rows (50% each)
        assert_eq!(rows.parse_shares_for_workspace(0), vec![0.5, 0.5]);
        assert_eq!(rows.parse_shares_for_workspace(1), vec![0.5, 0.5]);

        // Workspace 3 (index 2) gets 3 rows
        let ws3_shares = rows.parse_shares_for_workspace(2);
        assert_eq!(ws3_shares.len(), 3);
        for s in ws3_shares {
            assert!((s - 1.0 / 3.0).abs() < 1e-4);
        }

        // Workspace 4 through 9 (indices 3..9) default to 1 row
        for ws_idx in 3..9 {
            assert_eq!(
                rows.parse_shares_for_workspace(ws_idx),
                vec![1.0],
                "Workspace index {} should default to 1 row",
                ws_idx
            );
        }
    }

    #[test]
    fn test_per_workspace_rows_toml_roundtrip() {
        let toml_str = r#"
            [layout.rows]
            gap = 12

            [[layout.rows.workspaces]]
            workspaces = [1, 2]
            split = ["1/2", "1/2"]

            [[layout.rows.workspaces]]
            workspace = 3
            split = ["40%", "30%", "30%"]
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.layout.rows.gap, 12);
        assert_eq!(config.layout.rows.workspaces.len(), 2);
        assert_eq!(config.layout.rows.workspaces[0].workspaces, vec![1, 2]);
        assert_eq!(config.layout.rows.workspaces[1].workspaces, vec![3]);

        // Workspace 1 and 2: 2 rows
        assert_eq!(
            config.layout.rows.parse_shares_for_workspace(0),
            vec![0.5, 0.5]
        );
        assert_eq!(
            config.layout.rows.parse_shares_for_workspace(1),
            vec![0.5, 0.5]
        );

        // Workspace 3: 3 rows
        assert_eq!(
            config.layout.rows.parse_shares_for_workspace(2),
            vec![0.4, 0.3, 0.3]
        );

        // Workspace 4: default to 1 row
        assert_eq!(config.layout.rows.parse_shares_for_workspace(3), vec![1.0]);

        // When global split is set alongside per-workspace overrides, unmatched workspaces fall back to global split.
        let toml_with_global = r#"
            [layout.rows]
            split = ["60%", "40%"]
            gap = 12

            [[layout.rows.workspaces]]
            workspace = 1
            split = ["100%"]
        "#;
        let cfg2: Config = toml::from_str(toml_with_global).unwrap();
        assert_eq!(cfg2.layout.rows.parse_shares_for_workspace(0), vec![1.0]);
        assert_eq!(
            cfg2.layout.rows.parse_shares_for_workspace(1),
            vec![0.6, 0.4]
        );
    }

    #[test]
    fn test_workspace_rows_deserialization_formats() {
        let toml_str = r#"
            [layout.rows]
            gap = 8

            [[layout.rows.workspaces]]
            workspaces = "1, 2"
            split = "60%, 40%"

            [[layout.rows.workspaces]]
            workspace = 4
            split = ["100%"]

            [[layout.rows.workspaces]]
            workspaces = "5-7"
            split = ["50%", "50%"]
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.layout.rows.workspaces[0].workspaces, vec![1, 2]);
        assert_eq!(config.layout.rows.workspaces[0].split, vec!["60%", "40%"]);
        assert_eq!(config.layout.rows.workspaces[1].workspaces, vec![4]);
        assert_eq!(config.layout.rows.workspaces[2].workspaces, vec![5, 6, 7]);

        assert_eq!(
            config.layout.rows.parse_shares_for_workspace(0),
            vec![0.6, 0.4]
        );
        assert_eq!(
            config.layout.rows.parse_shares_for_workspace(4),
            vec![0.5, 0.5]
        );
        assert_eq!(config.layout.rows.parse_shares_for_workspace(7), vec![1.0]);
    }

    #[test]
    fn test_layout_rows_toml_roundtrip() {
        let toml_str = r#"
            [layout.rows]
            split = ["70%", "30%"]
            gap = 12
        "#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.layout.rows.split, vec!["70%", "30%"]);
        assert_eq!(config.layout.rows.gap, 12);
        let shares = config.layout.rows.parse_shares();
        assert_eq!(shares.len(), 2);
        assert!((shares[0] - 0.7).abs() < 1e-6);
        assert!((shares[1] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn test_compiled_window_rule_row_validation() {
        let mut config = Config {
            window_rules: vec![
                WindowRule {
                    match_class: Some("A".to_string()),
                    row: Some(2), // 1-based 2 -> 0-based 1
                    ..Default::default()
                },
                WindowRule {
                    match_class: Some("B".to_string()),
                    row: Some(0), // 0 violates 1-based, dropped
                    ..Default::default()
                },
                WindowRule {
                    match_class: Some("C".to_string()),
                    action: WindowAction::Float,
                    row: Some(2), // float ignores row
                    ..Default::default()
                },
                WindowRule {
                    match_class: Some("D".to_string()),
                    row: Some(6), // > MAX_ROWS (4), clamped to 4 -> 0-based 3
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let compiled = config.compile_window_rules();
        assert_eq!(compiled[0].row, Some(1));
        assert_eq!(compiled[1].row, None);
        assert_eq!(compiled[2].row, None);
        assert_eq!(compiled[3].row, Some(3));

        let warnings = config.validate();
        assert!(warnings.iter().any(|w| w.field == "window_rules[1].row"));
        assert!(warnings.iter().any(|w| w.field == "window_rules[2].row"));
        assert!(warnings.iter().any(|w| w.field == "window_rules[3].row"));
        assert_eq!(config.window_rules[1].row, None);
        assert_eq!(config.window_rules[2].row, None);
        assert_eq!(config.window_rules[3].row, Some(4));
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

    #[test]
    fn center_columns_migrates_legacy_and_roundtrips_through_settings() {
        use CenterColumnsConfig::*;
        for (fields, expected) in [
            ("", Off),
            ("center_single_column = false", Off),
            ("center_single_column = true", Single),
            ("center_columns = \"fit\"", Fit),
            ("center_single_column = true\ncenter_columns = \"off\"", Off),
            (
                "center_single_column = false\ncenter_columns = \"single\"",
                Single,
            ),
        ] {
            let config: Config = toml::from_str(&format!("[layout]\n{fields}\n")).unwrap();
            assert_eq!(
                config.layout.center_columns.center_columns, expected,
                "{fields}"
            );
            let json = serde_json::to_value(&config).unwrap();
            let settings_config: Config = serde_json::from_value(json).unwrap();
            let saved = toml::to_string_pretty(&settings_config).unwrap();
            assert!(!saved.contains("center_single_column"));
            let parsed: Config = toml::from_str(&saved).unwrap();
            assert_eq!(parsed.layout.center_columns.center_columns, expected);
        }
    }

    #[test]
    fn test_taskbar_buttons_config_migration_and_precedence() {
        use TaskbarButtons::*;
        assert_eq!(Config::default().behavior.taskbar_buttons, HideOffscreen);
        for (fields, expected) in [
            ("", HideOffscreen),
            ("hide_offscreen_taskbar_buttons = true", HideOffscreen),
            ("hide_offscreen_taskbar_buttons = false", ShowAll),
            ("taskbar_buttons = \"hide_offscreen\"", HideOffscreen),
            ("taskbar_buttons = \"hide_inactive_workspaces\"", HideInactiveWorkspaces),
            ("taskbar_buttons = \"show_all\"", ShowAll),
            ("hide_offscreen_taskbar_buttons = true\ntaskbar_buttons = \"show_all\"", ShowAll),
            ("hide_offscreen_taskbar_buttons = false\ntaskbar_buttons = \"hide_inactive_workspaces\"", HideInactiveWorkspaces),
            ("taskbar_buttons = \"hide_offscreen\"\nhide_offscreen_taskbar_buttons = false", HideOffscreen),
        ] {
            let config: Config = toml::from_str(&format!("[behavior]\n{fields}\n")).unwrap();
            assert_eq!(config.behavior.taskbar_buttons, expected, "{fields}");
            let saved = toml::to_string_pretty(&config).unwrap();
            let parsed: Config = toml::from_str(&saved).unwrap();
            assert_eq!(parsed.behavior.taskbar_buttons, expected, "{fields}");
            assert!(!saved.contains("hide_offscreen_taskbar_buttons"));
        }
        for fields in [
            "taskbar_buttons = \"unknown\"",
            "taskbar_buttons = true",
            "hide_offscreen_taskbar_buttons = \"false\"",
            "hide_offscreen_taskbar_buttons = true\ntaskbar_buttons = \"unknown\"",
        ] {
            assert!(
                toml::from_str::<Config>(&format!("[behavior]\n{fields}\n")).is_err(),
                "{fields}"
            );
        }
    }

    #[test]
    fn test_taskbar_buttons_settings_json_saves_only_canonical_key() {
        for mode in ["hide_offscreen", "hide_inactive_workspaces", "show_all"] {
            let config: Config = serde_json::from_value(serde_json::json!({
                "behavior": {
                    "taskbar_buttons": mode,
                    "hide_offscreen_taskbar_buttons": false,
                    "mouse_follows_focus": true,
                    "log_level": "warn"
                }
            }))
            .unwrap();
            let saved: toml::Value =
                toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
            assert_eq!(saved["behavior"]["taskbar_buttons"].as_str(), Some(mode));
            assert!(!saved["behavior"]
                .as_table()
                .unwrap()
                .contains_key("hide_offscreen_taskbar_buttons"));
            assert_eq!(
                saved["behavior"]["mouse_follows_focus"].as_bool(),
                Some(true)
            );
            assert_eq!(saved["behavior"]["log_level"].as_str(), Some("warn"));
            let loaded = serde_json::to_value(&config).unwrap();
            assert_eq!(loaded["behavior"]["taskbar_buttons"], mode);
            assert!(loaded["behavior"]
                .get("hide_offscreen_taskbar_buttons")
                .is_none());
        }
    }

    #[test]
    fn test_validate_hex_color_valid() {
        for color in ["ff0000", "FF0000"] {
            let mut config = Config::default();
            config.appearance.active_border_color = color.to_string();
            let warnings = config.validate();
            assert_eq!(config.appearance.active_border_color, color);
            assert!(!warnings
                .iter()
                .any(|w| w.field == "appearance.active_border_color"));
        }
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
        assert_eq!(
            config.appearance.active_border_color,
            default_active_border_color()
        );
        assert!(warnings
            .iter()
            .any(|w| w.field == "appearance.active_border_color"));
    }

    #[test]
    fn test_empty_config_uses_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.layout.gap, 10);
        assert_eq!(config.layout.outer_gap_left, 10);
        assert_eq!(config.layout.width_presets, vec![0.333, 0.5, 0.667]);
        assert_eq!(config.appearance.active_border_color, "4285F4");
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
        assert_eq!(config.layout.gap, 0);
        assert_eq!(config.layout.outer_gap_left, 0);
        assert_eq!(config.layout.outer_gap_right, 0);
        assert_eq!(config.layout.outer_gap_top, 0);
        assert_eq!(config.layout.outer_gap_bottom, 0);
        assert!(!warnings.iter().any(|w| matches!(
            w.field.as_str(),
            "layout.gap"
                | "layout.outer_gap_left"
                | "layout.outer_gap_right"
                | "layout.outer_gap_top"
                | "layout.outer_gap_bottom"
        )));
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
    fn test_regex_size_limit_rejects_oversized_pattern() {
        let config = Config {
            window_rules: vec![
                WindowRule {
                    match_class: Some("[a-z]{100000}".into()),
                    match_executable: Some("oversized-class.exe".into()),
                    ..Default::default()
                },
                WindowRule {
                    match_title: Some("[a-z]{100000}".into()),
                    match_executable: Some("oversized-title.exe".into()),
                    ..Default::default()
                },
                WindowRule {
                    match_class: Some("^SmallClass$".into()),
                    match_title: Some("^Small Title$".into()),
                    match_executable: Some("small.exe".into()),
                    action: WindowAction::Float,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let compiled = config.compile_window_rules();
        for executable in ["oversized-class.exe", "oversized-title.exe"] {
            assert!(!compiled
                .iter()
                .any(|rule| rule.match_executable.as_deref() == Some(executable)));
        }
        let small = compiled
            .iter()
            .find(|rule| rule.match_executable.as_deref() == Some("small.exe"))
            .unwrap();
        assert!(small.matches("SmallClass", "Small Title", "small.exe", None));
        assert_eq!(small.action, WindowAction::Float);
    }

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
    fn test_unknown_keys_are_ignored() {
        // serde(default) without deny_unknown_fields means extra keys are silently ignored
        let toml_str = r#"
            totally_unknown_section = "hello"
            [layout]
            gap = 20
            nonexistent_field = true
            unknown_key = "hello"
            another_unknown = 42
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
    fn test_file_reload_validation_reports_floating_dependency_and_clamps() {
        let dir = std::env::temp_dir().join(format!(
            "leopardwm_test_floating_reload_{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "[behavior]\nfloating_above_tiled = true\ntrack_focus_changes = false\n[layout]\ngap = -1\n",
        )
        .unwrap();
        let mut loaded = Config::load_from_path(&path).unwrap();
        let warnings = loaded.validate();
        assert_eq!(
            warnings
                .iter()
                .filter(|w| w.field == "behavior.floating_above_tiled")
                .count(),
            1
        );
        assert!(warnings.iter().any(|w| w.field == "layout.gap"));
        assert_eq!(loaded.layout.gap, 0);
        assert!(loaded.behavior.floating_above_tiled);
        assert!(!loaded.behavior.track_focus_changes);
        fs::remove_dir_all(&dir).unwrap();
    }

    struct ConfigFileFixture(PathBuf);

    impl ConfigFileFixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("leopardwm-config-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for ConfigFileFixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn unknown_keys_are_reported_and_compatibility_keys_are_accepted() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        fs::write(
            &path,
            r#"
            typo = true
            [layout]
            center_single_colum = true
            center_single_column = true
            outer_gap = 8
            default_column_width = 200
            min_column_width = 100
            max_column_width = 800
            [layout.rows]
            bogus = true
            [[layout.rows.workspaces]]
            workspace = 1
            split = ["100%"]
            [behavior]
            hide_offscreen_taskbar_buttons = true
            [gestures]
            raw_input = false
            diagnostic_capture_secs = 0
            [[window_rules]]
            match_clas = "example"
        "#,
        )
        .unwrap();
        let mut file = ConfigFile::default();
        file.load(std::slice::from_ref(&path)).unwrap();
        assert_eq!(
            file.unknown_keys,
            [
                "layout.center_single_colum",
                "layout.rows.bogus",
                "typo",
                "window_rules[0].match_clas"
            ]
        );
        fs::write(&path, "[layout]\ngap = 8\n").unwrap();
        file.load(std::slice::from_ref(&path)).unwrap();
        assert!(file.unknown_keys.is_empty());
    }

    #[test]
    fn unknown_keys_follow_successful_saves_but_not_failed_saves() {
        use std::os::windows::fs::OpenOptionsExt;

        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        fs::write(&path, "[layout]\ncenter_single_colum = true\n").unwrap();
        let mut file = ConfigFile::default();
        let config = file.load(std::slice::from_ref(&path)).unwrap();
        assert_eq!(file.unknown_keys, ["layout.center_single_colum"]);
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        assert!(file.save(&config, std::slice::from_ref(&path)).is_err());
        assert_eq!(file.unknown_keys, ["layout.center_single_colum"]);
        drop(lock);
        file.save(&config, std::slice::from_ref(&path)).unwrap();
        assert!(file.unknown_keys.is_empty());
        assert!(!fs::read_to_string(&path)
            .unwrap()
            .contains("center_single_colum ="));
    }

    #[test]
    fn unknown_keys_survive_failed_reload_but_clear_for_missing_file() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        fs::write(&path, "[layout]\ncenter_single_colum = true\n").unwrap();
        let mut file = ConfigFile::default();
        file.load(std::slice::from_ref(&path)).unwrap();
        assert_eq!(file.unknown_keys, ["layout.center_single_colum"]);
        fs::write(&path, "[layout\n").unwrap();
        assert!(file.load(std::slice::from_ref(&path)).is_err());
        assert_eq!(file.unknown_keys, ["layout.center_single_colum"]);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(file.load(std::slice::from_ref(&path)).is_err());
        assert_eq!(file.unknown_keys, ["layout.center_single_colum"]);
        fs::remove_dir(&path).unwrap();
        file.load(std::slice::from_ref(&path)).unwrap();
        assert!(file.unknown_keys.is_empty());
    }

    #[test]
    fn default_template_has_no_unknown_keys() {
        let content = leopardwm_ipc::config_template::render_default_config();
        let config = Config::parse(&content).unwrap();
        assert_eq!(
            find_unknown_config_keys(&content, &config, &[]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn recovered_config_still_reports_unknown_keys() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        fs::write(&path, "[layout]\ngap = 'bad'\ncenter_single_colum = true\n").unwrap();
        let mut file = ConfigFile::default();
        file.load(std::slice::from_ref(&path)).unwrap();
        assert!(!file.rejections.is_empty());
        assert_eq!(file.unknown_keys, ["layout.center_single_colum"]);
    }

    #[test]
    fn recovered_rule_indices_remain_aligned_after_a_rule_is_dropped() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        fs::write(&path, "[[window_rules]]\nwidth = 'bad'\n[[window_rules]]\nmatch_class = 'second'\nmatch_clas = 'typo'\n").unwrap();
        let mut file = ConfigFile::default();
        let config = file.load(std::slice::from_ref(&path)).unwrap();
        assert_eq!(config.window_rules.len(), 1);
        assert_eq!(file.unknown_keys, ["window_rules[1].match_clas"]);
    }

    #[test]
    fn recovery_handles_same_errors_containers_datetimes_and_rule_elements() {
        let cases: &[(&str, &[&str])] = &[
            ("[behavior]\nfocus_new_windows = 123\ntrack_focus_changes = 123\n", &["behavior.focus_new_windows", "behavior.track_focus_changes"]),
            ("[layout]\ngap = { x = 1 }\n", &["layout.gap"]),
            ("[layout]\ngap = {}\n", &["layout.gap"]),
            ("[layout]\ngap = 1979-05-27T07:32:00Z\n", &["layout.gap"]),
            ("mystery_timestamp = 1979-05-27T07:32:00Z\n[layout]\ngap = 'bad'\n", &["layout.gap"]),
            ("[[window_rules]]\nmatch_class = 'First'\nwidth = 'bad'\n[[window_rules]]\nmatch_class = 'Second'\nwidth = 321\n", &["window_rules[0]"]),
        ];
        for (source, expected) in cases {
            let fixture = ConfigFileFixture::new();
            let path = fixture.0.join("config.toml");
            fs::write(
                &path,
                format!("{source}\n[appearance]\nactive_border_width = 7\n"),
            )
            .unwrap();
            let mut file = ConfigFile::default();
            let config = file
                .load(std::slice::from_ref(&path))
                .unwrap_or_else(|error| panic!("{source}: {error:#}"));
            assert_eq!(config.appearance.active_border_width, 7, "{source}");
            let mut actual: Vec<_> = file.rejections[0]
                .recovered_fields
                .iter()
                .map(|field| field.path.as_str())
                .collect();
            actual.sort_unstable();
            assert_eq!(&actual, expected, "{source}");
            if source.contains("[[window_rules]]") {
                assert_eq!(config.window_rules.len(), 1);
                assert_eq!(
                    config.window_rules[0].match_class.as_deref(),
                    Some("Second")
                );
                assert_eq!(config.window_rules[0].width, Some(321));
            }
        }
    }

    #[test]
    fn invalid_behavior_field_recovers_and_preserves_original() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        let original = b"[layout]\ngap = 42\n[behavior]\nmove_step = 'bad'\n[[window_rules]]\nmatch_class = 'Example'\naction = 'float'\n";
        fs::write(&path, original).unwrap();
        let mut file = ConfigFile::default();
        let config = file.load(std::slice::from_ref(&path)).unwrap();
        assert_eq!(config.layout.gap, 42);
        assert_eq!(
            config.behavior.move_step,
            Config::default().behavior.move_step
        );
        assert_eq!(
            config.window_rules[0].match_class.as_deref(),
            Some("Example")
        );
        let fields = &file.rejections[0].recovered_fields;
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "behavior.move_step");
        assert_eq!(fields[0].location, Some((4, 13)));
        assert!(fields[0].reason.contains("invalid type"));
        file.save(&config, std::slice::from_ref(&path)).unwrap();
        assert_eq!(fs::read(corrupt_path(&path, ".corrupt")).unwrap(), original);
        assert!(file.load(std::slice::from_ref(&path)).is_ok());
        assert!(file.rejections.is_empty());
    }

    #[test]
    fn invalid_match_title_drops_entire_rule() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        let source = "[[window_rules]]\nmatch_class = 'Chrome'\nmatch_title = 123\naction = 'ignore'\n[[window_rules]]\nmatch_class = 'Other'\naction = 'float'\n";
        fs::write(&path, source).unwrap();
        let mut file = ConfigFile::default();
        let config = file.load(std::slice::from_ref(&path)).unwrap();
        assert_eq!(config.window_rules.len(), 1);
        assert_eq!(config.window_rules[0].match_class.as_deref(), Some("Other"));
        assert_eq!(
            file.rejections[0].recovered_fields[0].path,
            "window_rules[0]"
        );
        file.save(&config, std::slice::from_ref(&path)).unwrap();
        let saved = Config::load_from_path(&path).unwrap();
        assert_eq!(saved.window_rules.len(), 1);
        assert_eq!(saved.window_rules[0].match_class.as_deref(), Some("Other"));
    }

    #[test]
    fn recovery_reports_original_indices_for_multiple_removed_rules() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        let source = "[[window_rules]]\nmatch_class = 'A'\n[[window_rules]]\nmatch_class = 'B'\nmatch_title = 123\n[[window_rules]]\nmatch_class = 'C'\n[[window_rules]]\nmatch_class = 'D'\nmatch_title = 456\n";
        fs::write(&path, source).unwrap();
        let mut file = ConfigFile::default();
        let config = file.load(std::slice::from_ref(&path)).unwrap();
        assert_eq!(config.window_rules.len(), 2);
        assert_eq!(config.window_rules[0].match_class.as_deref(), Some("A"));
        assert_eq!(config.window_rules[1].match_class.as_deref(), Some("C"));
        let paths: Vec<_> = file.rejections[0]
            .recovered_fields
            .iter()
            .map(|field| field.path.as_str())
            .collect();
        assert_eq!(paths, ["window_rules[1]", "window_rules[3]"]);
    }

    #[test]
    fn recovery_drops_invalid_row_rule_without_widening_it() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        let source = "[[layout.rows.workspaces]]\nworkspace = 1\nsplit = 123\n[[layout.rows.workspaces]]\nworkspace = 2\nsplit = ['50%', '50%']\n";
        fs::write(&path, source).unwrap();
        let mut file = ConfigFile::default();
        let config = file.load(std::slice::from_ref(&path)).unwrap();
        assert_eq!(config.layout.rows.workspaces.len(), 1);
        assert_eq!(config.layout.rows.workspaces[0].workspaces, [2]);
        assert_eq!(
            file.rejections[0].recovered_fields[0].path,
            "layout.rows.workspaces[0]"
        );
    }

    #[test]
    fn invalid_window_rule_field_does_not_discard_other_rules() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        fs::write(&path, "[[window_rules]]\nmatch_class = 'First'\nwidth = 'bad'\n[[window_rules]]\nmatch_class = 'Second'\nwidth = 321\n").unwrap();
        let mut file = ConfigFile::default();
        let config = file.load(std::slice::from_ref(&path)).unwrap();
        assert_eq!(config.window_rules.len(), 1);
        assert_eq!(
            config.window_rules[0].match_class.as_deref(),
            Some("Second")
        );
        assert_eq!(config.window_rules[0].width, Some(321));
        assert_eq!(
            file.rejections[0].recovered_fields[0].path,
            "window_rules[0]"
        );
    }

    #[test]
    fn multiple_invalid_behavior_fields_recover_but_syntax_errors_do_not() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        fs::write(
            &path,
            "[behavior]\nmove_step = 'bad'\nfocus_new_windows = 123\n[appearance]\nactive_border = 'bad'\n",
        )
        .unwrap();
        let mut file = ConfigFile::default();
        let config = file.load(std::slice::from_ref(&path)).unwrap();
        assert_eq!(
            config.behavior.move_step,
            Config::default().behavior.move_step
        );
        assert_eq!(
            config.behavior.focus_new_windows,
            Config::default().behavior.focus_new_windows
        );
        let fields = &file.rejections[0].recovered_fields;
        assert_eq!(fields.len(), 3);
        assert!(fields
            .iter()
            .any(|field| field.path == "appearance.active_border"));
        assert!(fields
            .iter()
            .any(|field| field.path == "behavior.move_step"));
        assert!(fields
            .iter()
            .any(|field| field.path == "behavior.focus_new_windows"));
        fs::write(&path, "[layout\ngap = 42\n").unwrap();
        assert!(file.load(std::slice::from_ref(&path)).is_err());
    }

    #[test]
    fn invalid_array_values_reset_their_containing_field() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        for (source, field) in [
            ("[layout]\ngap = []\n", "layout.gap"),
            (
                "[layout]\nwidth_presets = [0.5, 'bad']\n",
                "layout.width_presets",
            ),
        ] {
            fs::write(&path, source).unwrap();
            let mut file = ConfigFile::default();
            assert!(file.load(std::slice::from_ref(&path)).is_ok());
            assert_eq!(file.rejections[0].recovered_fields[0].path, field);
        }
    }

    #[test]
    fn recovery_is_bounded_and_preserves_last_allowed_field() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        let mut file = ConfigFile::default();
        let rule = "[[window_rules]]\nwidth = 'bad'\n";
        fs::write(&path, rule.repeat(64)).unwrap();
        let config = file.load(std::slice::from_ref(&path)).unwrap();
        assert!(config.window_rules.is_empty());
        assert_eq!(file.rejections[0].recovered_fields.len(), 64);
        fs::write(&path, rule.repeat(65)).unwrap();
        assert!(file.load(std::slice::from_ref(&path)).is_err());
        assert!(file.rejections.last().unwrap().recovered_fields.is_empty());
    }

    #[test]
    fn rejected_config_is_preserved_before_save() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        let original = b"[layout]\ngap = [\n";
        fs::write(&path, original).unwrap();
        let mut file = ConfigFile::default();
        assert!(file.load(std::slice::from_ref(&path)).is_err());
        assert!(file.rejections[0].location.is_some());
        file.save(&Config::default(), std::slice::from_ref(&path))
            .unwrap();
        assert_eq!(fs::read(corrupt_path(&path, ".corrupt")).unwrap(), original);
        assert!(Config::load_from_path(&path).is_ok());
        assert_eq!(
            file.rejections[0].preserved_at,
            Some(corrupt_path(&path, ".corrupt"))
        );
        assert!(file.load(std::slice::from_ref(&path)).is_ok());
        assert!(file.rejections.is_empty());
    }

    #[test]
    fn rejected_config_backup_slots_fail_closed_or_reuse_identical_bytes() {
        for (first, second, expected) in [
            (Some(&b"different"[..]), None, Some(".corrupt.1")),
            (None, Some(&b"broken = ["[..]), Some(".corrupt.1")),
            (
                Some(&b"broken = ["[..]),
                Some(&b"different"[..]),
                Some(".corrupt"),
            ),
            (
                Some(&b"different"[..]),
                Some(&b"broken = ["[..]),
                Some(".corrupt.1"),
            ),
            (Some(&b"different"[..]), Some(&b"also different"[..]), None),
        ] {
            let fixture = ConfigFileFixture::new();
            let path = fixture.0.join("config.toml");
            let original = b"broken = [";
            fs::write(&path, original).unwrap();
            if let Some(bytes) = first {
                fs::write(corrupt_path(&path, ".corrupt"), bytes).unwrap();
            }
            if let Some(bytes) = second {
                fs::write(corrupt_path(&path, ".corrupt.1"), bytes).unwrap();
            }
            let mut file = ConfigFile::default();
            assert!(file.load(std::slice::from_ref(&path)).is_err());
            let result = file.save(&Config::default(), std::slice::from_ref(&path));
            if let Some(suffix) = expected {
                result.unwrap();
                assert_eq!(fs::read(corrupt_path(&path, suffix)).unwrap(), original);
                assert_eq!(
                    file.rejections[0].preserved_at,
                    Some(corrupt_path(&path, suffix))
                );
                if first.is_none() {
                    assert!(!corrupt_path(&path, ".corrupt").exists());
                }
            } else {
                assert!(result.is_err());
                assert_eq!(fs::read(&path).unwrap(), original);
            }
        }
    }

    #[test]
    fn successive_rejected_reloads_are_preserved() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        let mut file = ConfigFile::default();
        fs::write(&path, "[layout]\ngap = 42\n").unwrap();
        assert!(file.load(std::slice::from_ref(&path)).is_ok());
        for bytes in [&b"first = ["[..], &b"second = ["[..]] {
            fs::write(&path, bytes).unwrap();
            assert!(file.load(std::slice::from_ref(&path)).is_err());
        }
        file.save(&Config::default(), std::slice::from_ref(&path))
            .unwrap();
        assert_eq!(
            fs::read(corrupt_path(&path, ".corrupt")).unwrap(),
            b"first = ["
        );
        assert_eq!(
            fs::read(corrupt_path(&path, ".corrupt.1")).unwrap(),
            b"second = ["
        );
    }

    #[test]
    fn config_save_failure_does_not_modify_destination() {
        use std::os::windows::fs::OpenOptionsExt;

        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        let original = b"[layout]\ngap = 42\n";
        fs::write(&path, original).unwrap();
        let mut file = ConfigFile::default();
        let config = file.load(std::slice::from_ref(&path)).unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        assert!(file.save(&config, std::slice::from_ref(&path)).is_err());
        drop(lock);
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[test]
    fn unreadable_config_cannot_be_saved_and_non_utf8_is_preserved() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        fs::create_dir(&path).unwrap();
        let mut file = ConfigFile::default();
        assert!(file.load(std::slice::from_ref(&path)).is_err());
        assert!(file
            .save(&Config::default(), std::slice::from_ref(&path))
            .is_err());
        assert!(path.is_dir());
        fs::remove_dir(&path).unwrap();
        let bytes = [0xff, 0xfe, 0x00];
        fs::write(&path, bytes).unwrap();
        let mut file = ConfigFile::default();
        assert!(file.load(std::slice::from_ref(&path)).is_err());
        assert_eq!(file.rejections[0].location, None);
        file.save(&Config::default(), std::slice::from_ref(&path))
            .unwrap();
        assert_eq!(fs::read(corrupt_path(&path, ".corrupt")).unwrap(), bytes);
    }

    #[test]
    fn unreadable_rejection_is_cleared_after_readable_recovery() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        fs::create_dir(&path).unwrap();
        let mut file = ConfigFile::default();
        assert!(file.load(std::slice::from_ref(&path)).is_err());
        fs::remove_dir(&path).unwrap();
        let original = "[layout]\ngap = 'bad'\n";
        fs::write(&path, original).unwrap();
        let config = file.load(std::slice::from_ref(&path)).unwrap();
        file.save(&config, std::slice::from_ref(&path)).unwrap();
        assert_eq!(
            fs::read(corrupt_path(&path, ".corrupt")).unwrap(),
            original.as_bytes()
        );
    }

    #[test]
    fn missing_unreadable_config_does_not_block_recovered_secondary_save() {
        let fixture = ConfigFileFixture::new();
        let paths = [
            fixture.0.join("primary.toml"),
            fixture.0.join("secondary.toml"),
        ];
        fs::create_dir(&paths[0]).unwrap();
        let original = b"[layout]\ngap = 'bad'\n";
        fs::write(&paths[1], original).unwrap();
        let mut file = ConfigFile::default();
        assert!(file.load(&paths).is_err());
        fs::remove_dir(&paths[0]).unwrap();
        let config = file.load(&paths).unwrap();
        file.save(&config, &paths).unwrap();
        assert_eq!(
            fs::read(corrupt_path(&paths[1], ".corrupt")).unwrap(),
            original
        );
        assert!(file.rejected_paths.contains(&paths[0]));
        assert!(Config::load_from_path(&paths[1]).is_ok());
    }

    #[test]
    fn save_updates_all_hard_links() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        let linked = fixture.0.join("linked.toml");
        fs::write(&path, "[layout]\ngap = 42\n").unwrap();
        fs::hard_link(&path, &linked).unwrap();
        let mut file = ConfigFile::default();
        let mut config = file.load(std::slice::from_ref(&path)).unwrap();
        config.layout.gap = 17;
        file.save(&config, std::slice::from_ref(&path)).unwrap();
        assert_eq!(Config::load_from_path(&path).unwrap().layout.gap, 17);
        assert_eq!(Config::load_from_path(&linked).unwrap().layout.gap, 17);
    }

    #[test]
    fn blocked_hard_link_writes_leave_no_backups() {
        use std::os::windows::fs::OpenOptionsExt;

        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        let linked = fixture.0.join("linked.toml");
        let original = b"[layout]\ngap = 42\n";
        fs::write(&path, original).unwrap();
        fs::hard_link(&path, &linked).unwrap();
        let mut file = ConfigFile::default();
        let config = file.load(std::slice::from_ref(&path)).unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        for _ in 0..3 {
            assert!(file.save(&config, std::slice::from_ref(&path)).is_err());
        }
        drop(lock);
        assert!(!fs::read_dir(&fixture.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .any(|entry| entry.extension().is_some_and(|ext| ext == "backup")));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read(&linked).unwrap(), original);
    }

    #[test]
    fn config_save_uses_loaded_path_and_existing_serializer() {
        let fixture = ConfigFileFixture::new();
        let paths = [
            fixture.0.join("primary.toml"),
            fixture.0.join("secondary.toml"),
        ];
        fs::write(&paths[1], "[layout]\ngap = 42\n").unwrap();
        let mut file = ConfigFile::default();
        let config = file.load(&paths).unwrap();
        let expected = toml::to_string_pretty(&config).unwrap();
        file.save(&config, &paths).unwrap();
        assert!(!paths[0].exists());
        assert_eq!(fs::read_to_string(&paths[1]).unwrap(), expected);
        assert_eq!(file.load(&paths).unwrap().layout.gap, 42);
    }

    #[test]
    fn save_preserves_intervening_edits_or_fails_closed() {
        for occupied in [false, true] {
            let fixture = ConfigFileFixture::new();
            let path = fixture.0.join("config.toml");
            let rejected = b"[layout]\ngap = [\n";
            let edited = b"[layout]\ngap = 32\n";
            fs::write(&path, rejected).unwrap();
            let mut file = ConfigFile::default();
            assert!(file.load(std::slice::from_ref(&path)).is_err());
            fs::write(&path, edited).unwrap();
            if occupied {
                fs::write(corrupt_path(&path, ".corrupt.1"), b"another backup").unwrap();
            }
            let result = file.save(&Config::default(), std::slice::from_ref(&path));
            assert_eq!(fs::read(corrupt_path(&path, ".corrupt")).unwrap(), rejected);
            if occupied {
                assert!(result.is_err());
                assert_eq!(fs::read(&path).unwrap(), edited);
            } else {
                result.unwrap();
                assert_eq!(fs::read(corrupt_path(&path, ".corrupt.1")).unwrap(), edited);
            }
        }
    }

    #[test]
    fn edits_after_preserved_save_are_backed_up_or_blocked() {
        for (source, recovered) in [("broken = [", false), ("[layout]\ngap = 'bad'\n", true)] {
            for event in [
                "save",
                "clean reload",
                "recovered reload",
                "reload and save",
                "missing reload",
            ] {
                let fixture = ConfigFileFixture::new();
                let path = fixture.0.join("config.toml");
                let paths = std::slice::from_ref(&path);
                fs::write(&path, source).unwrap();
                let mut file = ConfigFile::default();
                assert_eq!(file.load(paths).is_ok(), recovered);
                file.save(&Config::default(), paths).unwrap();
                assert_eq!(
                    file.rejections[0].preserved_at,
                    Some(corrupt_path(&path, ".corrupt"))
                );
                match event {
                    "clean reload" => {
                        file.load(paths).unwrap();
                    }
                    "recovered reload" => {
                        fs::write(&path, "[layout]\ngap = 'bad'\n").unwrap();
                        file.load(paths).unwrap();
                    }
                    "reload and save" => {
                        let config = file.load(paths).unwrap();
                        file.save(&config, paths).unwrap();
                    }
                    "missing reload" => {
                        fs::remove_file(&path).unwrap();
                        let config = file.load(paths).unwrap();
                        file.save(&config, paths).unwrap();
                    }
                    "save" => {}
                    _ => unreachable!(),
                }
                let first_edit = "[layout]\ngap = 21\n";
                fs::write(&path, first_edit).unwrap();
                let result = file.save(&Config::default(), paths);
                if event == "recovered reload" && !recovered {
                    assert!(result.is_err(), "{source:?} / {event}");
                    assert_eq!(fs::read(&path).unwrap(), first_edit.as_bytes());
                } else {
                    result.unwrap();
                    assert_eq!(
                        fs::read(corrupt_path(&path, ".corrupt.1")).unwrap(),
                        first_edit.as_bytes(),
                        "{source:?} / {event}"
                    );
                    let second_edit = "[layout]\ngap = 31\n";
                    fs::write(&path, second_edit).unwrap();
                    assert!(file.save(&Config::default(), paths).is_err());
                    assert_eq!(fs::read(&path).unwrap(), second_edit.as_bytes());
                }
                assert_eq!(
                    fs::read(corrupt_path(&path, ".corrupt")).unwrap(),
                    source.as_bytes()
                );
            }
        }
    }

    #[test]
    fn archived_backup_stays_gone_until_a_new_rejection() {
        for (source, recovered) in [("broken = [", false), ("[layout]\ngap = 'bad'\n", true)] {
            let fixture = ConfigFileFixture::new();
            let path = fixture.0.join("config.toml");
            let backup = corrupt_path(&path, ".corrupt");
            fs::write(&path, source).unwrap();
            let mut file = ConfigFile::default();
            assert_eq!(file.load(std::slice::from_ref(&path)).is_ok(), recovered);
            file.save(&Config::default(), std::slice::from_ref(&path))
                .unwrap();
            fs::remove_file(&backup).unwrap();
            file.save(&Config::default(), std::slice::from_ref(&path))
                .unwrap();
            assert!(!backup.exists());
            assert_eq!(file.rejections[0].preserved_at, Some(backup.clone()));
            fs::write(&path, source).unwrap();
            assert_eq!(file.load(std::slice::from_ref(&path)).is_ok(), recovered);
            file.save(&Config::default(), std::slice::from_ref(&path))
                .unwrap();
            assert_eq!(fs::read(&backup).unwrap(), source.as_bytes());
        }
    }

    #[test]
    fn repeated_saves_after_rejection_do_not_preserve_own_writes() {
        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        let rejected = b"broken = [";
        fs::write(&path, rejected).unwrap();
        let mut file = ConfigFile::default();
        assert!(file.load(std::slice::from_ref(&path)).is_err());
        let mut config = Config::default();
        for gap in [10, 20, 30, 40] {
            config.layout.gap = gap;
            file.save(&config, std::slice::from_ref(&path)).unwrap();
            assert_eq!(Config::load_from_path(&path).unwrap().layout.gap, gap);
            assert_eq!(fs::read(corrupt_path(&path, ".corrupt")).unwrap(), rejected);
            assert!(!corrupt_path(&path, ".corrupt.1").exists());
        }
    }

    #[test]
    #[ignore = "needs SeCreateSymbolicLinkPrivilege"]
    fn save_follows_config_file_symlink() {
        use std::os::windows::fs::symlink_file;

        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        let target = fixture.0.join("dotfiles.toml");
        fs::write(&target, "[layout]\ngap = 42\n").unwrap();
        if let Err(error) = symlink_file(&target, &path) {
            if error.raw_os_error() == Some(1314) {
                panic!("symlink test requires SeCreateSymbolicLinkPrivilege: {error}");
            }
            panic!("failed to create test symlink: {error}");
        }
        let mut file = ConfigFile::default();
        let mut config = file.load(std::slice::from_ref(&path)).unwrap();
        config.layout.gap = 17;
        file.save(&config, std::slice::from_ref(&path)).unwrap();
        assert!(fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(Config::load_from_path(&target).unwrap().layout.gap, 17);
    }

    #[test]
    #[ignore = "needs SeCreateSymbolicLinkPrivilege"]
    fn save_creates_missing_symlink_target() {
        use std::os::windows::fs::symlink_file;

        let fixture = ConfigFileFixture::new();
        let path = fixture.0.join("config.toml");
        let target = fixture.0.join("missing.toml");
        if let Err(error) = symlink_file(&target, &path) {
            if error.raw_os_error() == Some(1314) {
                panic!("symlink test requires SeCreateSymbolicLinkPrivilege: {error}");
            }
            panic!("failed to create test symlink: {error}");
        }
        let mut file = ConfigFile::default();
        let mut config = Config::default();
        config.layout.gap = 17;
        file.save(&config, std::slice::from_ref(&path)).unwrap();
        assert!(fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(Config::load_from_path(&target).unwrap().layout.gap, 17);

        fs::write(&target, "broken = [").unwrap();
        assert!(file.load(std::slice::from_ref(&path)).is_err());
        fs::remove_file(&target).unwrap();
        file.save(&config, std::slice::from_ref(&path)).unwrap();
        let backup = corrupt_path(&path, ".corrupt");
        assert_eq!(fs::read(&backup).unwrap(), b"broken = [");
        fs::remove_file(&backup).unwrap();
        file.save(&config, std::slice::from_ref(&path)).unwrap();
        assert!(!backup.exists());
    }

    #[test]
    fn save_rechecks_backup_after_failed_replace_and_rejection() {
        use std::os::windows::fs::OpenOptionsExt;

        for reload in [false, true] {
            let fixture = ConfigFileFixture::new();
            let path = fixture.0.join("config.toml");
            let original = b"broken = [";
            fs::write(&path, original).unwrap();
            let mut file = ConfigFile::default();
            assert!(file.load(std::slice::from_ref(&path)).is_err());
            let lock = fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(&path)
                .unwrap();
            assert!(file
                .save(&Config::default(), std::slice::from_ref(&path))
                .is_err());
            drop(lock);
            fs::remove_file(corrupt_path(&path, ".corrupt")).unwrap();
            if reload {
                assert!(file.load(std::slice::from_ref(&path)).is_err());
            }
            file.save(&Config::default(), std::slice::from_ref(&path))
                .unwrap();
            assert_eq!(fs::read(corrupt_path(&path, ".corrupt")).unwrap(), original);
        }
    }

    #[test]
    fn missing_config_clears_rejection_and_returns_to_primary_path() {
        let fixture = ConfigFileFixture::new();
        let paths = [
            fixture.0.join("primary.toml"),
            fixture.0.join("secondary.toml"),
        ];
        fs::create_dir(&paths[1]).unwrap();
        let mut file = ConfigFile::default();
        assert!(file.load(&paths).is_err());
        fs::remove_dir(&paths[1]).unwrap();
        assert!(file.load(&paths).is_ok());
        file.save(&Config::default(), &paths).unwrap();
        assert!(paths[0].is_file());
        assert!(!paths[1].exists());
        assert!(file.rejections.is_empty());
    }

    #[test]
    fn test_config_save_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "leopardwm_test_save_roundtrip_{}",
            std::process::id()
        ));
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
