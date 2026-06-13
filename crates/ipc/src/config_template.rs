//! Single source of truth for the generated config-file template shared by
//! the daemon (first-run config) and the CLI (`config init` / profiles).

/// Per-profile values spliced into the shared config template; `None` keeps the default.
#[derive(Debug, Clone, Copy, Default)]
pub struct TemplateOverrides {
    pub gap: Option<i32>,
    pub outer_gap: Option<i32>,
    pub centering_mode: Option<&'static str>,
    pub profile_name: Option<&'static str>,
}

/// Render the default commented config template (no overrides).
pub fn render_default_config() -> String {
    render_config(&TemplateOverrides::default())
}

/// Render the commented config template with the given profile overrides applied.
pub fn render_config(overrides: &TemplateOverrides) -> String {
    let gap = overrides.gap.unwrap_or(10);
    let outer_gap = overrides.outer_gap.unwrap_or(10);
    let centering_mode = overrides.centering_mode.unwrap_or("center");
    let header = match overrides.profile_name {
        Some(name) => format!("# LeopardWM Configuration — {name} profile"),
        None => "# LeopardWM Configuration".to_string(),
    };
    let hotkeys = super::hotkeys::render_template_block();

    format!(
        r#"{header}
# https://github.com/jcardama/LeopardWM

[layout]
# Gap between columns in pixels
gap = {gap}

# Outer gaps at the edges of the viewport in pixels
outer_gap_left = {outer_gap}
outer_gap_right = {outer_gap}
outer_gap_top = {outer_gap}
outer_gap_bottom = {outer_gap}

# Width presets (fractions of usable viewport width).
# First preset is used as the default width for new columns.
width_presets = [0.333, 0.5, 0.667]

# Height presets (fractions of column height / weight).
height_presets = [0.333, 0.5, 0.667]

# Centering mode: "center", "just_in_view", or "on_overflow"
# - center: Always center the focused column
# - just_in_view: Only scroll if focused column would be outside viewport
# - on_overflow: Center only when the column is wider than the viewport
centering_mode = "{centering_mode}"

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

# Disable Windows 11 Snap Layouts for tiled windows (also disables maximize button)
# disable_snap_layouts = false

# Check GitHub Releases once a day for a newer version. One anonymous HTTPS GET to
# api.github.com on startup + every 24h. Disable to skip entirely.
# check_for_updates = false

# Animate Chromium / Electron / Mozilla / Cascadia windows via DWM thumbnails
# instead of per-frame SetWindowPos during column scrolls. Eliminates the
# 1px wobble and renderer stutter on swap-chain-sensitive apps (Chrome,
# Edge, Slack, Discord, Beeper, Spotify, VS Code, Firefox, Windows Terminal).
# Default on since v0.1.18. Set to false to fall back to the legacy
# per-frame SetWindowPos path.
# swap_chain_ghost_animation = false

# Where newly opened windows go: "new_column" (default, own column to the
# right) or "in_column" (stacked into the focused column).
# new_window_placement = "new_column"

[hotkeys]
{hotkeys}
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

[animation]
# Animation timing. Durations in milliseconds; 0 = snap instantly.
# easing accepts "linear" | "ease_in" | "ease_out" | "ease_in_out".
layout_duration_ms = 150            # column move / resize / tab changes
workspace_switch_duration_ms = 200  # switching workspaces
scroll_duration_ms = 200            # scrolling a column into view
overview_duration_ms = 150          # overview open/close zoom
easing = "ease_out"

[overview]
# Workspace overview card bodies: "live" (default) shows DWM window
# previews (last frame for windows on hidden workspaces); "snapshot"
# shows a frame captured right before each window left the screen
# (icon until one exists); "placeholder" keeps the static app-icon
# bodies.
# render = "live"

[workspaces]
# Optional display names for workspaces 1-9, by position. Shown in
# `lwm query workspace` and pushed to bars over IPC. Leave an entry empty
# ("") to keep that workspace's number. Omit the list to name nothing.
# names = ["web", "code", "chat", "media"]

# Built-in example: Firefox / Zen Picture-in-Picture popups draw their own
# square frame, so we override the focus-border corner style to match. Edit
# or remove freely — corner_style accepts "square" | "rounded" | "small_rounded".
[[window_rules]]
match_class = "MozillaDialogClass"
corner_style = "square"

# [[window_rules]]
# match_class = "Chrome_WidgetWin_1"
# match_title = ".*DevTools.*"
# action = "float"

# Per-app open behavior: open on a workspace (1-9), set the initial column
# width (viewport fraction), or open with the column maximized.
# [[window_rules]]
# match_executable = "spotify.exe"
# open_on_workspace = 5
# column_width = 0.5
# open_maximized = false
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_default_config_has_all_sections() {
        let config = render_default_config();
        for section in [
            "[layout]",
            "[appearance]",
            "[behavior]",
            "[hotkeys]",
            "[gestures]",
            "[snap_hints]",
            "[animation]",
            "[overview]",
            "[workspaces]",
            "[[window_rules]]",
        ] {
            assert!(config.contains(section), "missing section: {section}");
        }
        assert!(config.starts_with("# LeopardWM Configuration\n"));
        assert!(config.contains("gap = 10"));
        assert!(config.contains("centering_mode = \"center\""));
        assert!(config.contains("\"Win+Ctrl+Escape\" = \"panic_revert\""));
    }

    #[test]
    fn test_render_config_applies_overrides() {
        let config = render_config(&TemplateOverrides {
            gap: Some(12),
            outer_gap: Some(16),
            centering_mode: Some("just_in_view"),
            profile_name: Some("ultrawide"),
        });
        assert!(config.starts_with("# LeopardWM Configuration — ultrawide profile\n"));
        assert!(config.contains("gap = 12"));
        assert!(config.contains("outer_gap_left = 16"));
        assert!(config.contains("outer_gap_bottom = 16"));
        assert!(config.contains("centering_mode = \"just_in_view\""));
    }
}
