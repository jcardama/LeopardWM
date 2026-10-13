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
        let is_tiled = self.find_window_location_rc(window_id).is_some();
        let is_floating = self.is_floating(window_id);
        if is_tiled || is_floating {
            if self.fullscreen_window == Some(window_id) {
                self.fullscreen_window = None;
            }
            // Clear stale min-size constraints for the minimized window and
            // its column siblings — the column geometry is about to change.
            self.window_min_widths.remove(&window_id);
            self.window_min_heights.remove(&window_id);
            if is_tiled {
                if let Some((row_idx, col_idx, _)) = self.find_window_location_rc(window_id) {
                    if let Some(col) = self.rows.get(row_idx).and_then(|r| r.columns.get(col_idx)) {
                        for &sibling in col.windows() {
                            if sibling != window_id {
                                self.window_min_widths.remove(&sibling);
                                self.window_min_heights.remove(&sibling);
                            }
                        }
                    }
                    if let Some(row) = self.rows.get_mut(row_idx) {
                        row.active_animation = None;
                    }
                }
            }
            self.minimized_windows.insert(window_id)
        } else {
            false
        }
    }

    /// Mark a window as restored (no longer minimized).
    pub fn mark_restored(&mut self, window_id: WindowId) -> bool {
        self.window_min_widths.remove(&window_id);
        self.window_min_heights.remove(&window_id);
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
    pub fn clear_fullscreen_if_window(&mut self, window_id: WindowId) -> bool {
        if self.fullscreen_window == Some(window_id) {
            self.fullscreen_window = None;
            self.window_min_widths.remove(&window_id);
            self.window_min_heights.remove(&window_id);
            true
        } else {
            false
        }
    }

    /// Toggle fullscreen mode for the focused window.
    pub fn toggle_fullscreen(&mut self) -> bool {
        if let Some(fs_wid) = self.fullscreen_window {
            self.window_min_widths.remove(&fs_wid);
            self.window_min_heights.remove(&fs_wid);

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

    /// Retarget fullscreen to the currently focused visible window if currently fullscreen.
    pub fn fullscreen_follow_focus(&mut self) -> bool {
        let Some(prev) = self.fullscreen_window else {
            return false;
        };
        let Some(wid) = self.focused_visible_window() else {
            return false;
        };
        if wid == prev {
            return false;
        }
        self.window_min_widths.remove(&prev);
        self.window_min_heights.remove(&prev);
        self.fullscreen_window = Some(wid);
        true
    }

    // ========================================================================
    // Toggle Floating
    // ========================================================================

    /// Toggle floating state for the focused window.
    pub fn toggle_floating(&mut self, viewport: Rect) -> Option<WindowId> {
        let wid = self.focused_window()?;

        self.clear_fullscreen_if_window(wid);

        let origin = self.find_window_location_rc(wid).map(|(row_idx, col_idx, _)| {
            let left_neighbor = if col_idx > 0 {
                self.rows[row_idx].columns[col_idx - 1].windows.first().copied()
            } else {
                None
            };
            (left_neighbor, col_idx, row_idx)
        });

        let _ = self.remove_window(wid);

        if let Some(origin) = origin {
            self.float_origin_column.insert(wid, origin);
        }

        let float_w = 800.min(viewport.width - 40);
        let float_h = 600.min(viewport.height - 40);
        let float_x = viewport.x + (viewport.width - float_w) / 2;
        let float_y = viewport.y + (viewport.height - float_h) / 2;
        let rect = Rect::new(float_x, float_y, float_w, float_h);

        let _ = self.add_floating(wid, rect);
        Some(wid)
    }

    /// Move a floating window back to the tiling layout.
    pub fn unfloat_window(&mut self, window_id: WindowId) -> bool {
        let origin = self.float_origin_column.remove(&window_id);
        if self.remove_floating(window_id) {
            if let Some((left_neighbor, fallback_col, fallback_row)) = origin {
                let (target_row, target_col) = if let Some(neighbor_id) = left_neighbor {
                    if let Some((r, c, _)) = self.find_window_location_rc(neighbor_id) {
                        (r, c + 1)
                    } else {
                        let r = fallback_row.min(self.rows.len().saturating_sub(1));
                        (r, fallback_col.min(self.rows[r].columns.len()))
                    }
                } else {
                    let r = fallback_row.min(self.rows.len().saturating_sub(1));
                    (r, 0)
                };
                let column_width = self.default_column_width.max(crate::MIN_COLUMN_WIDTH);
                let column = Column::new(window_id, column_width);
                let target_col = target_col.min(self.rows[target_row].columns.len());
                self.insert_column_at_row(column, target_row, target_col);
                self.focused_row = target_row;
                self.rows[target_row].focused_column = target_col;
                self.rows[target_row].focused_window_in_column = 0;
            } else {
                let _ = self.insert_window(window_id, None);
            }
            true
        } else {
            false
        }
    }
}
