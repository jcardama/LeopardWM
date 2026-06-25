//! Sticky windows: pin a window visible on every workspace.
//!
//! A sticky window is kept floating and re-homed to the active workspace
//! whenever the workspace changes, so it appears to follow you everywhere.
//! Session-scoped: the set is keyed by HWND and is not persisted across a
//! daemon restart.

use tracing::{info, warn};

use crate::state::AppState;

impl AppState {
    /// Toggle "sticky" for the OS-focused window. Pinning floats the window
    /// (so it can overlay any workspace) and adds it to the sticky set;
    /// un-pinning removes it from the set and leaves it floating in place.
    pub(crate) fn toggle_sticky(&mut self) {
        // Use the OS-foreground window: a sticky window is floating, which
        // `Workspace::focused_window` does not report.
        let Some(wid) = self
            .previous_focused_hwnd
            .or_else(|| self.focused_workspace().and_then(|ws| ws.focused_window()))
        else {
            info!("Sticky: no focused window");
            return;
        };

        if self.sticky_windows.remove(&wid) {
            // Un-pin: stop following workspaces, but leave it floating.
            if let Some((mon, ws_idx)) = self.find_window_workspace(wid) {
                if let Some(ws) = self.workspaces.get_mut(&mon).and_then(|v| v.get_mut(ws_idx)) {
                    ws.set_floating_pinned(wid, false);
                }
            }
            info!("Sticky: unpinned window {}", wid);
            return;
        }

        #[cfg(not(test))]
        if !leopardwm_platform_win32::is_window_valid(wid) {
            info!("Sticky: focused window {} is no longer valid", wid);
            return;
        }

        // Stick in the window's CURRENT mode — never force a mode change. A
        // floating window is pinned so it overlays any workspace; a tiled
        // window stays tiled and is re-homed as a column on each switch.
        if let Some(ws) = self.focused_workspace_mut() {
            if ws.is_floating(wid) {
                ws.set_floating_pinned(wid, true);
            }
        }
        self.sticky_windows.insert(wid);
        let _ = self.apply_layout();
        self.sync_foreground_window();
        info!("Sticky: pinned window {}", wid);
    }

    /// Move every sticky window onto the focused monitor's active workspace
    /// (preserving its floating rect), so pinned windows follow workspace
    /// switches. Drops any sticky window that no longer exists. Call this
    /// after changing the active workspace, before the layout is applied.
    pub(crate) fn rehome_sticky_windows(&mut self) {
        if self.sticky_windows.is_empty() {
            return;
        }
        let monitor = self.focused_monitor;
        let active = self.active_workspace_idx(monitor);
        // Sort for a stable order: multiple tiled stickies must keep a fixed
        // end-of-strip order across switches rather than reshuffle (HashSet
        // iteration is unordered).
        let mut sticky: Vec<u64> = self.sticky_windows.iter().copied().collect();
        sticky.sort_unstable();
        for wid in sticky {
            let Some((mon, ws_idx)) = self.find_window_workspace(wid) else {
                // No longer tracked anywhere (closed); drop it.
                self.sticky_windows.remove(&wid);
                continue;
            };
            // Only follow workspace switches on the window's own monitor.
            // A sticky window on another monitor stays put there (it remains
            // visible on that monitor) rather than being yanked across.
            if mon != monitor || ws_idx == active {
                continue;
            }
            // Branch on the window's mode in ITS OWN (source) workspace, never
            // the now-active one — else a floating sticky could be misread as
            // tiled and wrongly converted.
            let is_floating = self
                .workspaces
                .get(&mon)
                .and_then(|v| v.get(ws_idx))
                .is_some_and(|ws| ws.is_floating(wid));

            if !is_floating {
                // Tiled sticky: capture its column width, remove from source,
                // then append as a column at the END of the active workspace
                // without stealing focus. Carrying the width keeps the window
                // from shrinking to the default on every workspace switch.
                let width = self
                    .workspaces
                    .get(&mon)
                    .and_then(|v| v.get(ws_idx))
                    .and_then(|ws| {
                        ws.find_window_location(wid)
                            .and_then(|(col, _)| ws.columns().get(col).map(|c| c.width()))
                    });
                let removed = self
                    .workspaces
                    .get_mut(&mon)
                    .and_then(|v| v.get_mut(ws_idx))
                    .map(|ws| ws.remove_window(wid).is_ok())
                    .unwrap_or(false);
                if !removed {
                    continue;
                }
                let appended = self
                    .workspaces
                    .get_mut(&monitor)
                    .and_then(|v| v.get_mut(active))
                    .map(|ws| ws.append_window_no_focus(wid, width).is_ok())
                    .unwrap_or(false);
                if !appended {
                    // Effectively unreachable (no duplicate possible post-remove);
                    // recover the window into its source workspace if it ever fires.
                    warn!("Sticky: could not re-home tiled window {}; rolled back to source", wid);
                    if let Some(ws) = self.workspaces.get_mut(&mon).and_then(|v| v.get_mut(ws_idx)) {
                        let _ = ws.insert_window(wid, None);
                    }
                }
                continue;
            }

            // Floating sticky: preserve its float rect across the move.
            let rect = self
                .workspaces
                .get(&mon)
                .and_then(|v| v.get(ws_idx))
                .and_then(|ws| ws.floating_windows().iter().find(|f| f.id == wid).map(|f| f.rect))
                .unwrap_or_else(|| self.centered_float_rect());
            // Add to the active workspace FIRST; only detach from the source
            // once that succeeds, so a failed move never loses the window.
            let added = self
                .workspaces
                .get_mut(&monitor)
                .and_then(|v| v.get_mut(active))
                .map(|ws| {
                    let ok = ws.add_floating(wid, rect).is_ok();
                    if ok {
                        ws.set_floating_pinned(wid, true);
                    }
                    ok
                })
                .unwrap_or(false);
            if !added {
                warn!("Sticky: could not re-home window {}; left on its workspace", wid);
                continue;
            }
            let detached = self
                .workspaces
                .get_mut(&mon)
                .and_then(|v| v.get_mut(ws_idx))
                .map(|ws| ws.remove_floating(wid) || ws.remove_window(wid).is_ok())
                .unwrap_or(false);
            if !detached {
                // Could not remove from the source after adding to the
                // destination. Roll back the add so the window never lives
                // in two workspaces at once.
                warn!("Sticky: re-home of window {} could not detach source; rolled back", wid);
                if let Some(ws) = self.workspaces.get_mut(&monitor).and_then(|v| v.get_mut(active)) {
                    ws.remove_floating(wid);
                }
            }
        }
    }

    /// Re-assert focus on a sticky window after a workspace switch, when
    /// the user was focused on it before the switch (mirrors
    /// `refocus_scratchpad_if_active`). The pin follows the switch, so
    /// focus must too. Both `sync_foreground_window` passes (immediate and
    /// animation-landing) prefer `previous_focused_hwnd` when it floats on
    /// the active workspace, so setting it here holds focus across both.
    /// Returns `true` if the refocus applied; no-op (`false`) unless the
    /// window is still sticky and floating on the now-active workspace.
    pub(crate) fn refocus_sticky_window(&mut self, wid: u64) -> bool {
        if !self.sticky_windows.contains(&wid) {
            return false;
        }
        let monitor = self.focused_monitor;
        let active = self.active_workspace_idx(monitor);
        let (floating_here, tiled_here) = self
            .workspaces
            .get(&monitor)
            .and_then(|v| v.get(active))
            .map(|ws| {
                let floating = ws.is_floating(wid);
                (floating, !floating && ws.contains_window(wid))
            })
            .unwrap_or((false, false));
        if !floating_here && !tiled_here {
            return false;
        }
        if tiled_here {
            // Align the workspace's tiled focus with the foreground we assert
            // so the post-switch focus sync doesn't recompute and fight it.
            if let Some(ws) = self.workspaces.get_mut(&monitor).and_then(|v| v.get_mut(active)) {
                let _ = ws.focus_window(wid);
            }
        }
        self.previous_focused_hwnd = Some(wid);
        self.show_border(wid);
        #[cfg(not(test))]
        {
            let _ = leopardwm_platform_win32::set_foreground_window(wid);
        }
        true
    }

    /// Drop a window from the sticky set when it is destroyed.
    pub(crate) fn sticky_on_window_destroyed(&mut self, wid: u64) {
        if self.sticky_windows.remove(&wid) {
            info!("Sticky: pinned window {} closed; unpinned", wid);
        }
    }
}
