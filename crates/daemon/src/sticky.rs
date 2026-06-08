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
            info!("Sticky: unpinned window {}", wid);
            return;
        }

        #[cfg(not(test))]
        if !leopardwm_platform_win32::is_window_valid(wid) {
            info!("Sticky: focused window {} is no longer valid", wid);
            return;
        }

        // Pin: ensure the window is floating so it can overlay any
        // workspace. Only mark it sticky once it is actually floating, so a
        // tiled window can't be pinned in a non-floating state.
        let viewport = self.focused_viewport();
        let floating = if let Some(ws) = self.focused_workspace_mut() {
            if !ws.is_floating(wid) {
                let _ = ws.focus_window(wid);
                let _ = ws.toggle_floating(viewport);
            }
            ws.is_floating(wid)
        } else {
            false
        };
        if !floating {
            warn!("Sticky: could not float window {}; not pinning", wid);
            return;
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
        let sticky: Vec<u64> = self.sticky_windows.iter().copied().collect();
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
            // Preserve its current floating rect across the move.
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
                .map(|ws| ws.add_floating(wid, rect).is_ok())
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

    /// Drop a window from the sticky set when it is destroyed.
    pub(crate) fn sticky_on_window_destroyed(&mut self, wid: u64) {
        if self.sticky_windows.remove(&wid) {
            info!("Sticky: pinned window {} closed; unpinned", wid);
        }
    }
}
