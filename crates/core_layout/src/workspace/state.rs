use crate::*;

use crate::workspace::Workspace;

impl Workspace {
    // ========================================================================
    // Minimize Methods
    // ========================================================================

    /// Mark a window as minimized. The window stays in its column (or floating
    /// list) but is excluded from layout placement calculations.
    ///
    /// If the minimized window is the current fullscreen window, fullscreen
    /// mode is exited so that other windows become visible again.
    ///
    /// Returns `true` if the window was managed and is now marked minimized.
    /// Returns `false` if the window is not in this workspace.
    pub fn mark_minimized(&mut self, window_id: WindowId) -> bool {
        let is_tiled = self.find_window_location(window_id).is_some();
        let is_floating = self.is_floating(window_id);
        if is_tiled || is_floating {
            if self.fullscreen_window == Some(window_id) {
                self.fullscreen_window = None;
            }
            // Cancel active animation — its target is now stale after minimize
            if is_tiled {
                self.active_animation = None;
            }
            self.minimized_windows.insert(window_id)
        } else {
            false
        }
    }

    /// Mark a window as restored (no longer minimized).
    ///
    /// Returns `true` if the window was previously marked minimized.
    pub fn mark_restored(&mut self, window_id: WindowId) -> bool {
        // Clear cached min-width — the window's size constraints may have
        // changed while minimized. It will be re-detected if still enforced.
        self.window_min_widths.remove(&window_id);
        self.minimized_windows.remove(&window_id)
    }

    /// Check if a window is currently minimized.
    pub fn is_minimized(&self, window_id: WindowId) -> bool {
        self.minimized_windows.contains(&window_id)
    }

    /// Get the number of currently minimized windows.
    pub fn minimized_count(&self) -> usize {
        self.minimized_windows.len()
    }

    // ========================================================================
    // Fullscreen Methods
    // ========================================================================

    /// Check if a window is currently fullscreen.
    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen_window.is_some()
    }

    /// Get the fullscreen window ID, if any.
    pub fn fullscreen_window_id(&self) -> Option<WindowId> {
        self.fullscreen_window
    }

    /// Clear fullscreen mode when it currently targets `window_id`.
    ///
    /// Returns `true` if fullscreen was cleared.
    pub fn clear_fullscreen_if_window(&mut self, window_id: WindowId) -> bool {
        if self.fullscreen_window == Some(window_id) {
            self.fullscreen_window = None;
            true
        } else {
            false
        }
    }

    /// Toggle fullscreen mode for the focused window.
    /// Returns true if entering fullscreen, false if exiting.
    pub fn toggle_fullscreen(&mut self) -> bool {
        if let Some(fs_wid) = self.fullscreen_window {
            // If fullscreen points at a removed/minimized window, clear stale state
            // and treat this invocation as a fresh toggle attempt.
            if !self.contains_window(fs_wid) || self.minimized_windows.contains(&fs_wid) {
                self.fullscreen_window = None;
            } else {
                self.fullscreen_window = None;
                return false;
            }
        }

        if let Some(wid) = self.focused_visible_window() {
            self.fullscreen_window = Some(wid);
            true
        } else {
            self.fullscreen_window = None;
            false
        }
    }

    // ========================================================================
    // Toggle Floating
    // ========================================================================

    /// Toggle floating state for the focused window.
    /// If the focused window is tiled, move it to floating with a centered rect.
    /// If the focused window is floating, this is a no-op (floating windows are not focused via column focus).
    /// Returns the window ID that was toggled, if any.
    pub fn toggle_floating(&mut self, viewport: Rect) -> Option<WindowId> {
        let wid = self.focused_window()?;

        // Defensive guard: if the focused window is currently fullscreen, clear
        // fullscreen before moving it to floating state.
        self.clear_fullscreen_if_window(wid);

        // Check if the focused window is tiled (it always is since focus tracks tiled columns)
        // Remove from columns
        let _ = self.remove_window(wid);

        // Center a floating window of 800x600 or clamped to viewport
        let float_w = 800.min(viewport.width - 40);
        let float_h = 600.min(viewport.height - 40);
        let float_x = viewport.x + (viewport.width - float_w) / 2;
        let float_y = viewport.y + (viewport.height - float_h) / 2;
        let rect = Rect::new(float_x, float_y, float_w, float_h);

        let _ = self.add_floating(wid, rect);
        Some(wid)
    }

    /// Move a floating window back to the tiling layout.
    /// Returns true if the window was unfloated.
    pub fn unfloat_window(&mut self, window_id: WindowId) -> bool {
        if self.remove_floating(window_id) {
            // Insert as a new column
            let _ = self.insert_window(window_id, None);
            true
        } else {
            false
        }
    }
}
