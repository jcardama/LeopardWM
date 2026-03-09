use crate::*;

use crate::workspace::Workspace;

impl Workspace {
    /// Compute placements for all windows given a viewport.
    ///
    /// Returns a list of WindowPlacement structs indicating where each window
    /// should be positioned and whether it's visible or off-screen.
    ///
    /// Note: Negative gaps are treated as zero for calculation purposes.
    pub fn compute_placements(&self, viewport: Rect) -> Vec<WindowPlacement> {
        // Use rounding instead of truncation to prevent sub-pixel jitter
        let viewport_left = self.scroll_offset.round() as i32;

        // Fullscreen mode: one window covers the entire viewport, others are off-screen
        if let Some(fs_wid) = self.fullscreen_window {
            return self.compute_fullscreen_placements(fs_wid, viewport, viewport_left);
        }

        self.compute_non_fullscreen_placements(viewport, viewport_left)
    }

    /// Compute non-fullscreen placements for a specific viewport-left offset.
    /// Used by both static and animated placement paths.
    fn compute_non_fullscreen_placements(
        &self,
        viewport: Rect,
        viewport_left: i32,
    ) -> Vec<WindowPlacement> {
        let mut placements = Vec::new();

        // Defensively clamp gaps to >= 0 in case fields were set directly
        let gap = self.gap.max(0);
        let outer_left = self.outer_gap_left.max(0);
        let outer_top = self.outer_gap_top.max(0);
        let outer_bottom = self.outer_gap_bottom.max(0);

        // Visible strip area inside viewport padding
        let vis_w = self.visible_width(viewport.width);
        let visible_right = viewport_left.saturating_add(vis_w);

        // Pre-compute effective column widths respecting window min-widths.
        // If a column contains a window with a known minimum width larger than
        // the allocated column width, widen it and shrink flexible columns.
        let effective_widths: Vec<i32> = {
            let mut widths: Vec<i32> = self.columns.iter().map(|c| c.width).collect();
            if !self.window_min_widths.is_empty() {
                let mut excess = 0i32;
                let mut flexible_total = 0i32;
                for (col_idx, column) in self.columns.iter().enumerate() {
                    if !self.is_column_active(column) {
                        continue;
                    }
                    let min_w = self.column_effective_min_width(column);
                    if min_w > column.width {
                        excess += min_w - column.width;
                        widths[col_idx] = min_w;
                    } else {
                        flexible_total += column.width;
                    }
                }
                if excess > 0 && flexible_total > 0 {
                    let mut remaining = excess;
                    for (col_idx, column) in self.columns.iter().enumerate() {
                        if !self.is_column_active(column) || widths[col_idx] != column.width {
                            continue;
                        }
                        let share = ((column.width as f64 / flexible_total as f64) * excess as f64)
                            .round() as i32;
                        let shrink = share.min(remaining).min(column.width - MIN_COLUMN_WIDTH).max(0);
                        widths[col_idx] -= shrink;
                        remaining -= shrink;
                    }
                }
            }
            widths
        };

        // Strip starts at 0 — outer gaps are viewport padding
        let mut current_x: i32 = 0;

        for (col_idx, column) in self.columns.iter().enumerate() {
            let eff_width = effective_widths[col_idx];

            // Calculate column position in strip coordinates
            let col_strip_x = current_x;
            let col_strip_right = col_strip_x.saturating_add(eff_width);

            // Transform to screen coordinates:
            // strip_x → screen_x = strip_x - scroll_offset + viewport.x + outer_left
            // Must use regular arithmetic (not saturating) to allow negative screen
            // positions for columns partially scrolled off the left edge.
            let col_screen_x = col_strip_x - viewport_left + viewport.x + outer_left;

            // Determine visibility against the visible strip area
            let visibility = if col_strip_right <= viewport_left {
                Visibility::OffScreenLeft
            } else if col_strip_x >= visible_right {
                Visibility::OffScreenRight
            } else {
                Visibility::Visible
            };

            // Filter out minimized windows, collecting (original_index, window_id) pairs
            let visible_windows: Vec<(usize, WindowId)> = column
                .windows()
                .iter()
                .enumerate()
                .filter(|(_, w)| !self.minimized_windows.contains(w))
                .map(|(i, &w)| (i, w))
                .collect();

            // Skip columns where all windows are minimized
            if !self.is_column_active(column) {
                continue;
            }

            // Build visible-window weights
            let usable_height = viewport
                .height
                .saturating_sub(outer_top)
                .saturating_sub(outer_bottom)
                .max(0);
            let window_count = visible_windows.len() as i32;
            let window_gaps = if window_count > 1 {
                gap.saturating_mul(window_count - 1)
            } else {
                0
            };
            let available_height = (usable_height - window_gaps).max(0);

            // Compute per-window heights using weights
            let visible_weights: Vec<f64> = if column.height_weights.len() == column.windows().len() {
                visible_windows.iter().map(|(i, _)| column.height_weights[*i]).collect()
            } else {
                vec![1.0; visible_windows.len()]
            };
            let weight_sum: f64 = visible_weights.iter().sum();
            let normalized: Vec<f64> = if weight_sum > 0.0 {
                visible_weights.iter().map(|w| w / weight_sum).collect()
            } else {
                vec![1.0 / visible_windows.len().max(1) as f64; visible_windows.len()]
            };

            let mut current_y = viewport.y + outer_top;

            for (win_idx, &(_, window_id)) in visible_windows.iter().enumerate() {
                let height = if win_idx == visible_windows.len() - 1 {
                    (viewport.y + viewport.height - outer_bottom - current_y).max(0)
                } else {
                    (available_height as f64 * normalized[win_idx]).round() as i32
                };

                placements.push(WindowPlacement {
                    window_id,
                    rect: Rect::new(col_screen_x, current_y, eff_width, height),
                    visibility,
                    column_index: col_idx,
                });

                current_y = current_y.saturating_add(height).saturating_add(gap);
            }

            current_x = current_x.saturating_add(eff_width).saturating_add(gap);
        }

        // Add floating windows (visible unless minimized, at their absolute positions)
        for floating in &self.floating_windows {
            if self.minimized_windows.contains(&floating.id) {
                continue;
            }
            placements.push(WindowPlacement {
                window_id: floating.id,
                rect: floating.rect,
                visibility: Visibility::Visible,
                column_index: usize::MAX, // Sentinel for floating windows
            });
        }

        placements
    }

    /// Compute placements for all windows, using animated scroll offset if active.
    ///
    /// This is similar to `compute_placements` but uses `effective_scroll_offset()`
    /// to support smooth scrolling animations.
    pub fn compute_placements_animated(&self, viewport: Rect) -> Vec<WindowPlacement> {
        // Use animated scroll offset
        let viewport_left = self.effective_scroll_offset().round() as i32;

        // Fullscreen mode: one window covers the entire viewport, others are off-screen
        if let Some(fs_wid) = self.fullscreen_window {
            return self.compute_fullscreen_placements(fs_wid, viewport, viewport_left);
        }

        self.compute_non_fullscreen_placements(viewport, viewport_left)
    }

    /// Compute placements when a window is fullscreen.
    /// The fullscreen window gets the full viewport; all others are marked off-screen.
    fn compute_fullscreen_placements(
        &self,
        fs_wid: WindowId,
        viewport: Rect,
        viewport_left: i32,
    ) -> Vec<WindowPlacement> {
        // Stale or minimized fullscreen target: fall back to normal placements.
        if !self.contains_window(fs_wid) || self.minimized_windows.contains(&fs_wid) {
            return self.compute_non_fullscreen_placements(viewport, viewport_left);
        }

        let mut placements = Vec::new();

        for (col_idx, column) in self.columns.iter().enumerate() {
            for &window_id in column.windows() {
                if self.minimized_windows.contains(&window_id) {
                    continue;
                }
                if window_id == fs_wid {
                    placements.push(WindowPlacement {
                        window_id,
                        rect: viewport,
                        visibility: Visibility::Visible,
                        column_index: col_idx,
                    });
                } else {
                    placements.push(WindowPlacement {
                        window_id,
                        rect: Rect::new(0, 0, 0, 0),
                        visibility: Visibility::OffScreenLeft,
                        column_index: col_idx,
                    });
                }
            }
        }

        // Floating windows are also hidden during fullscreen
        for floating in &self.floating_windows {
            if self.minimized_windows.contains(&floating.id) {
                continue;
            }
            if floating.id == fs_wid {
                placements.push(WindowPlacement {
                    window_id: floating.id,
                    rect: viewport,
                    visibility: Visibility::Visible,
                    column_index: usize::MAX,
                });
            } else {
                placements.push(WindowPlacement {
                    window_id: floating.id,
                    rect: Rect::new(0, 0, 0, 0),
                    visibility: Visibility::OffScreenLeft,
                    column_index: usize::MAX,
                });
            }
        }

        placements
    }
}
