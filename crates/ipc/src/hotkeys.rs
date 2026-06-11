//! Single source of truth for hotkey actions: default binding, human
//! label, and display grouping for each bindable action.
//!
//! Both the daemon (default config, settings UI) and the CLI
//! (`generate-config`) derive everything from [`hotkey_catalog`], so a new
//! hotkey is defined exactly once instead of in `default_bindings`, the
//! config template, the CLI template, and the settings JS tables.

use std::collections::HashMap;

use serde::Serialize;

/// A bindable action plus its presentation metadata.
#[derive(Debug, Clone, Serialize)]
pub struct HotkeyAction {
    /// Action id used in config files and `parse_command`.
    pub id: String,
    /// Default key chord, or `None` for actions with no default binding.
    #[serde(rename = "key")]
    pub default_key: Option<String>,
    /// Human-readable label for the settings UI.
    pub label: String,
    /// Section grouping (drives settings order and config-template comments).
    #[serde(skip)]
    pub group: &'static str,
}

fn action(id: &str, default_key: Option<&str>, label: &str, group: &'static str) -> HotkeyAction {
    HotkeyAction {
        id: id.to_string(),
        default_key: default_key.map(str::to_string),
        label: label.to_string(),
        group,
    }
}

/// The ordered catalog of every bindable action. Order here is the display
/// order in the settings UI; `group` drives section headers there and in
/// the generated config template.
pub fn hotkey_catalog() -> Vec<HotkeyAction> {
    let mut v = vec![
        action("focus_left", Some("Ctrl+Alt+H"), "Focus left", "Focus"),
        action("focus_right", Some("Ctrl+Alt+L"), "Focus right", "Focus"),
        action("focus_up", Some("Ctrl+Alt+K"), "Focus up", "Focus"),
        action("focus_down", Some("Ctrl+Alt+J"), "Focus down", "Focus"),
        action("move_column_left", Some("Ctrl+Alt+Shift+H"), "Move column left", "Move column"),
        action("move_column_right", Some("Ctrl+Alt+Shift+L"), "Move column right", "Move column"),
        action(
            "move_window_left",
            Some("Ctrl+Alt+["),
            "Move window left",
            "Move window to adjacent column",
        ),
        action(
            "move_window_right",
            Some("Ctrl+Alt+]"),
            "Move window right",
            "Move window to adjacent column",
        ),
        action(
            "expel_to_left",
            Some("Ctrl+Alt+Shift+["),
            "Expel to left",
            "Expel window to a new column",
        ),
        action(
            "expel_to_right",
            Some("Ctrl+Alt+Shift+]"),
            "Expel to right",
            "Expel window to a new column",
        ),
        action(
            "consume_from_left",
            Some("Ctrl+Alt+,"),
            "Consume from left",
            "Stack the neighbor's window into the focused column",
        ),
        action(
            "consume_from_right",
            Some("Ctrl+Alt+."),
            "Consume from right",
            "Stack the neighbor's window into the focused column",
        ),
        action("move_window_up", Some("Ctrl+Alt+Shift+K"), "Move window up", "Move window within column"),
        action(
            "move_window_down",
            Some("Ctrl+Alt+Shift+J"),
            "Move window down",
            "Move window within column",
        ),
        action("cycle_width_down", Some("Ctrl+Alt+-"), "Cycle width down", "Column width"),
        action("cycle_width_up", Some("Ctrl+Alt+="), "Cycle width up", "Column width"),
        action("equalize_widths", Some("Ctrl+Alt+0"), "Equalize widths", "Column width"),
        action("cycle_height_down", Some("Ctrl+Alt+Shift+-"), "Cycle height down", "Window height"),
        action("cycle_height_up", Some("Ctrl+Alt+Shift+="), "Cycle height up", "Window height"),
        action("equalize_heights", Some("Ctrl+Alt+Shift+0"), "Equalize heights", "Window height"),
        action("focus_monitor_left", Some("Ctrl+Alt+Win+,"), "Focus monitor left", "Monitor focus"),
        action(
            "focus_monitor_right",
            Some("Ctrl+Alt+Win+."),
            "Focus monitor right",
            "Monitor focus",
        ),
        action(
            "move_to_monitor_left",
            Some("Ctrl+Alt+Win+Shift+,"),
            "Move to monitor left",
            "Move window to monitor",
        ),
        action(
            "move_to_monitor_right",
            Some("Ctrl+Alt+Win+Shift+."),
            "Move to monitor right",
            "Move window to monitor",
        ),
        action("center_column", Some("Ctrl+Alt+C"), "Center column", "Column layout"),
        action("maximize_column", Some("Ctrl+Alt+M"), "Maximize column", "Column layout"),
        action("close_window", Some("Ctrl+Alt+W"), "Close window", "Window"),
        action("toggle_floating", Some("Ctrl+Alt+F"), "Toggle floating", "Window"),
        action("toggle_fullscreen", Some("Ctrl+Alt+Shift+F"), "Toggle fullscreen", "Window"),
        action("toggle_tabbed", Some("Ctrl+Alt+T"), "Toggle tabbed column", "Window"),
        action("scratchpad_toggle", Some("Ctrl+Alt+S"), "Toggle scratchpad", "Window"),
        action("scratchpad_stash", Some("Ctrl+Alt+Shift+S"), "Stash to scratchpad", "Window"),
        action("toggle_sticky", Some("Ctrl+Alt+Y"), "Toggle sticky", "Window"),
        action(
            "toggle_new_window_placement",
            None,
            "Toggle new-window placement (new column / in column)",
            "Window",
        ),
        action("toggle_pause", Some("Ctrl+Alt+P"), "Toggle pause", "Session"),
        action("refresh", Some("Ctrl+Alt+R"), "Refresh", "Session"),
        action("reload", Some("Ctrl+Alt+Shift+R"), "Reload config", "Session"),
        action("panic_revert", Some("Win+Ctrl+Escape"), "Emergency restore", "Session"),
        action("toggle_overview", Some("Ctrl+Alt+Space"), "Toggle overview", "Workspaces"),
        action("workspace_prev", Some("Win+Ctrl+Left"), "Previous workspace", "Workspaces"),
        action("workspace_next", Some("Win+Ctrl+Right"), "Next workspace", "Workspaces"),
    ];
    for i in 1..=9u8 {
        v.push(action(
            &format!("switch_workspace_{i}"),
            Some(&format!("Ctrl+Alt+{i}")),
            &format!("Workspace {i}"),
            "Switch to workspace",
        ));
    }
    for i in 1..=9u8 {
        v.push(action(
            &format!("move_to_workspace_{i}"),
            Some(&format!("Ctrl+Alt+Shift+{i}")),
            &format!("Move to Workspace {i}"),
            "Move window to workspace",
        ));
    }
    v
}

/// Build the default key-chord -> action-id map from the catalog (skipping
/// actions without a default binding).
pub fn default_bindings_map() -> HashMap<String, String> {
    hotkey_catalog()
        .into_iter()
        .filter_map(|a| a.default_key.map(|key| (key, a.id)))
        .collect()
}

/// Map a normalized (lowercase, underscored) action id to its `IpcCommand`.
/// Covers every catalog id plus non-catalog command aliases (gestures,
/// renames, deprecated width presets) accepted in config files.
pub fn command_for_action(id: &str) -> Option<crate::IpcCommand> {
    use crate::IpcCommand;

    if let Some(suffix) = id.strip_prefix("switch_workspace_") {
        let index: u8 = suffix.parse().ok()?;
        return (1..=9)
            .contains(&index)
            .then_some(IpcCommand::SwitchWorkspace { index });
    }
    if let Some(suffix) = id.strip_prefix("move_to_workspace_") {
        let index: u8 = suffix.parse().ok()?;
        return (1..=9)
            .contains(&index)
            .then_some(IpcCommand::MoveToWorkspace { index });
    }

    Some(match id {
        "focus_left" => IpcCommand::FocusLeft,
        "focus_right" => IpcCommand::FocusRight,
        "focus_up" => IpcCommand::FocusUp,
        "focus_down" => IpcCommand::FocusDown,
        "focus_next" => IpcCommand::FocusNext,
        "focus_prev" => IpcCommand::FocusPrev,
        "move_column_left" => IpcCommand::MoveColumnLeft,
        "move_column_right" => IpcCommand::MoveColumnRight,
        "focus_monitor_left" => IpcCommand::FocusMonitorLeft,
        "focus_monitor_right" => IpcCommand::FocusMonitorRight,
        "move_to_monitor_left" => IpcCommand::MoveWindowToMonitorLeft,
        "move_to_monitor_right" => IpcCommand::MoveWindowToMonitorRight,
        "cycle_width_up" | "resize_grow" => IpcCommand::CycleWidthUp,
        "cycle_width_down" | "resize_shrink" => IpcCommand::CycleWidthDown,
        "cycle_height_up" => IpcCommand::CycleHeightUp,
        "cycle_height_down" => IpcCommand::CycleHeightDown,
        "equalize_heights" => IpcCommand::EqualizeColumnHeights,
        "scroll_left" => IpcCommand::Scroll { delta: -100.0 },
        "scroll_right" => IpcCommand::Scroll { delta: 100.0 },
        "refresh" => IpcCommand::Refresh,
        "reload" => IpcCommand::Reload,
        "panic_revert" => IpcCommand::PanicRevert,
        "toggle_pause" => IpcCommand::TogglePause,
        "close_window" => IpcCommand::CloseWindow,
        "toggle_floating" => IpcCommand::ToggleFloating,
        "toggle_fullscreen" => IpcCommand::ToggleFullscreen,
        "scratchpad_stash" => IpcCommand::ScratchpadStash,
        "scratchpad_toggle" => IpcCommand::ScratchpadToggle,
        "toggle_sticky" => IpcCommand::ToggleSticky,
        "toggle_new_window_placement" => IpcCommand::ToggleNewWindowPlacement,
        "toggle_tabbed" => IpcCommand::ToggleTabbed,
        "width_third" => IpcCommand::SetColumnWidth { fraction: 0.333 },
        "width_half" => IpcCommand::SetColumnWidth { fraction: 0.5 },
        "width_two_thirds" => IpcCommand::SetColumnWidth { fraction: 0.667 },
        "center_column" => IpcCommand::CenterColumn,
        "maximize_column" => IpcCommand::MaximizeColumn,
        "equalize_widths" => IpcCommand::EqualizeColumnWidths,
        "move_window_left" => IpcCommand::MoveWindowLeft,
        "move_window_right" => IpcCommand::MoveWindowRight,
        "expel_to_left" => IpcCommand::ExpelToLeft,
        "expel_to_right" => IpcCommand::ExpelToRight,
        "consume_from_left" => IpcCommand::ConsumeFromLeft,
        "consume_from_right" => IpcCommand::ConsumeFromRight,
        "move_window_up" => IpcCommand::MoveWindowUp,
        "move_window_down" => IpcCommand::MoveWindowDown,
        "workspace_prev" => IpcCommand::WorkspacePrev,
        "workspace_next" => IpcCommand::WorkspaceNext,
        "toggle_overview" => IpcCommand::ToggleOverview,
        _ => return None,
    })
}

/// Render the `[hotkeys]` binding lines for the generated config template,
/// grouped with a `# <group>` comment before each section. Does not emit
/// the `[hotkeys]` header or `scroll_modifier` line.
pub fn render_template_block() -> String {
    let mut out = String::new();
    let mut current_group = "";
    for a in hotkey_catalog() {
        let Some(key) = a.default_key else { continue };
        if a.group != current_group {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("# {}\n", a.group));
            current_group = a.group;
        }
        out.push_str(&format!("\"{}\" = \"{}\"\n", key, a.id));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalog_has_no_duplicate_ids_or_keys() {
        let catalog = hotkey_catalog();
        let mut ids = std::collections::HashSet::new();
        let mut keys = std::collections::HashSet::new();
        for a in &catalog {
            assert!(ids.insert(a.id.clone()), "duplicate action id: {}", a.id);
            if let Some(ref k) = a.default_key {
                assert!(keys.insert(k.clone()), "duplicate default key: {}", k);
            }
        }
    }

    #[test]
    fn test_default_bindings_map_matches_catalog() {
        let map = default_bindings_map();
        let bound = hotkey_catalog()
            .into_iter()
            .filter(|a| a.default_key.is_some())
            .count();
        assert_eq!(map.len(), bound);
        assert_eq!(map.get("Ctrl+Alt+H"), Some(&"focus_left".to_string()));
        assert_eq!(map.get("Ctrl+Alt+,"), Some(&"consume_from_left".to_string()));
        assert_eq!(map.get("Ctrl+Alt+5"), Some(&"switch_workspace_5".to_string()));
    }

    #[test]
    fn test_default_bindings_match_frozen_expected_set() {
        // Golden set: the complete key-chord -> action map the catalog must
        // produce. Guards against transcription drift (a typo would keep the
        // count at 57 but change a binding). Update deliberately when the
        // intended defaults change.
        let expected: &[(&str, &str)] = &[
            ("Ctrl+Alt+H", "focus_left"),
            ("Ctrl+Alt+L", "focus_right"),
            ("Ctrl+Alt+K", "focus_up"),
            ("Ctrl+Alt+J", "focus_down"),
            ("Ctrl+Alt+Shift+H", "move_column_left"),
            ("Ctrl+Alt+Shift+L", "move_column_right"),
            ("Ctrl+Alt+[", "move_window_left"),
            ("Ctrl+Alt+]", "move_window_right"),
            ("Ctrl+Alt+Shift+[", "expel_to_left"),
            ("Ctrl+Alt+Shift+]", "expel_to_right"),
            ("Ctrl+Alt+,", "consume_from_left"),
            ("Ctrl+Alt+.", "consume_from_right"),
            ("Ctrl+Alt+Shift+K", "move_window_up"),
            ("Ctrl+Alt+Shift+J", "move_window_down"),
            ("Ctrl+Alt+-", "cycle_width_down"),
            ("Ctrl+Alt+=", "cycle_width_up"),
            ("Ctrl+Alt+0", "equalize_widths"),
            ("Ctrl+Alt+Shift+-", "cycle_height_down"),
            ("Ctrl+Alt+Shift+=", "cycle_height_up"),
            ("Ctrl+Alt+Shift+0", "equalize_heights"),
            ("Ctrl+Alt+Win+,", "focus_monitor_left"),
            ("Ctrl+Alt+Win+.", "focus_monitor_right"),
            ("Ctrl+Alt+Win+Shift+,", "move_to_monitor_left"),
            ("Ctrl+Alt+Win+Shift+.", "move_to_monitor_right"),
            ("Ctrl+Alt+C", "center_column"),
            ("Ctrl+Alt+M", "maximize_column"),
            ("Ctrl+Alt+W", "close_window"),
            ("Ctrl+Alt+F", "toggle_floating"),
            ("Ctrl+Alt+Shift+F", "toggle_fullscreen"),
            ("Ctrl+Alt+T", "toggle_tabbed"),
            ("Ctrl+Alt+S", "scratchpad_toggle"),
            ("Ctrl+Alt+Shift+S", "scratchpad_stash"),
            ("Ctrl+Alt+Y", "toggle_sticky"),
            ("Ctrl+Alt+P", "toggle_pause"),
            ("Ctrl+Alt+R", "refresh"),
            ("Ctrl+Alt+Shift+R", "reload"),
            ("Win+Ctrl+Escape", "panic_revert"),
            ("Ctrl+Alt+Space", "toggle_overview"),
            ("Win+Ctrl+Left", "workspace_prev"),
            ("Win+Ctrl+Right", "workspace_next"),
            ("Ctrl+Alt+1", "switch_workspace_1"),
            ("Ctrl+Alt+2", "switch_workspace_2"),
            ("Ctrl+Alt+3", "switch_workspace_3"),
            ("Ctrl+Alt+4", "switch_workspace_4"),
            ("Ctrl+Alt+5", "switch_workspace_5"),
            ("Ctrl+Alt+6", "switch_workspace_6"),
            ("Ctrl+Alt+7", "switch_workspace_7"),
            ("Ctrl+Alt+8", "switch_workspace_8"),
            ("Ctrl+Alt+9", "switch_workspace_9"),
            ("Ctrl+Alt+Shift+1", "move_to_workspace_1"),
            ("Ctrl+Alt+Shift+2", "move_to_workspace_2"),
            ("Ctrl+Alt+Shift+3", "move_to_workspace_3"),
            ("Ctrl+Alt+Shift+4", "move_to_workspace_4"),
            ("Ctrl+Alt+Shift+5", "move_to_workspace_5"),
            ("Ctrl+Alt+Shift+6", "move_to_workspace_6"),
            ("Ctrl+Alt+Shift+7", "move_to_workspace_7"),
            ("Ctrl+Alt+Shift+8", "move_to_workspace_8"),
            ("Ctrl+Alt+Shift+9", "move_to_workspace_9"),
        ];
        let expected_map: HashMap<String, String> = expected
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(default_bindings_map(), expected_map);
    }

    #[test]
    fn test_every_catalog_action_has_command_mapping() {
        // Drift guard: a new catalog row without a command_for_action
        // mapping fails here instead of silently parsing to None.
        for a in hotkey_catalog() {
            assert!(
                command_for_action(&a.id).is_some(),
                "catalog action '{}' has no command_for_action mapping",
                a.id
            );
        }
    }

    #[test]
    fn test_command_for_action_workspace_suffixes() {
        use crate::IpcCommand;
        assert_eq!(
            command_for_action("switch_workspace_5"),
            Some(IpcCommand::SwitchWorkspace { index: 5 })
        );
        assert_eq!(
            command_for_action("move_to_workspace_9"),
            Some(IpcCommand::MoveToWorkspace { index: 9 })
        );
        assert_eq!(command_for_action("switch_workspace_0"), None);
        assert_eq!(command_for_action("switch_workspace_10"), None);
        assert_eq!(command_for_action("move_to_workspace_x"), None);
    }

    #[test]
    fn test_template_block_groups_and_binds() {
        let block = render_template_block();
        assert!(block.contains("# Focus\n"));
        assert!(block.contains("\"Ctrl+Alt+H\" = \"focus_left\"\n"));
        assert!(block.contains("\"Ctrl+Alt+,\" = \"consume_from_left\"\n"));
        // Numbered workspaces are present.
        assert!(block.contains("\"Ctrl+Alt+1\" = \"switch_workspace_1\""));
    }
}
