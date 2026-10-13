use crate::*;

use crate::workspace::Workspace;

impl Workspace {
    /// Calculate the sub-viewport rectangles for each row in the workspace.
    pub fn row_rects(&self, viewport: Rect) -> Vec<Rect> {
        let num_rows = self.rows.len();
        if num_rows == 0 {
            return Vec::new();
        }

        let row_gap = self.row_gap.max(0);
        let outer_top = self.outer_gap_top.max(0);
        let outer_bottom = self.outer_gap_bottom.max(0);
        let total_row_gaps = row_gap.saturating_mul(num_rows.saturating_sub(1) as i32);
        let available_height = viewport
            .height
            .saturating_sub(outer_top)
            .saturating_sub(outer_bottom)
            .saturating_sub(total_row_gaps)
            .max(0);

        let total_shares: f64 = self.row_shares.iter().map(|&s| s.max(0.0)).sum();
        let total_shares = if total_shares > 0.0 {
            total_shares
        } else {
            num_rows as f64
        };

        let mut row_heights = Vec::with_capacity(num_rows);
        let mut allocated_height = 0;
        for i in 0..num_rows {
            if i == num_rows - 1 {
                row_heights.push((available_height - allocated_height).max(0));
            } else {
                let share = self.row_shares.get(i).copied().unwrap_or(1.0).max(0.0);
                let h = ((available_height as f64) * (share / total_shares)).round() as i32;
                let h = h.max(0);
                allocated_height += h;
                row_heights.push(h);
            }
        }

        let mut rects = Vec::with_capacity(num_rows);
        let mut cur_y = viewport.y + outer_top;
        for h in row_heights {
            rects.push(Rect::new(viewport.x, cur_y, viewport.width, h));
            cur_y = cur_y.saturating_add(h).saturating_add(row_gap);
        }
        rects
    }

    /// Compute placements for all windows given a viewport.
    ///
    /// Returns a list of WindowPlacement structs indicating where each window
    /// should be positioned and whether it's visible or off-screen.
    ///
    /// Note: Negative gaps are treated as zero for calculation purposes.
    pub fn compute_placements(&self, viewport: Rect) -> Vec<WindowPlacement> {
        let offsets: Vec<i32> = self
            .rows
            .iter()
            .map(|r| r.scroll_offset.round() as i32)
            .collect();

        // Fullscreen mode: one window covers the entire viewport, others are off-screen
        if let Some(fs_wid) = self.fullscreen_window {
            return self.compute_fullscreen_placements(fs_wid, viewport, &offsets);
        }

        self.compute_non_fullscreen_placements(viewport, &offsets)
    }

    /// Compute placements for the ENTIRE strip, ignoring scroll: viewport
    /// left is pinned to 0 for all rows and the caller supplies a viewport wide enough
    /// for every column, so nothing is marked off-screen. Used by the
    /// workspace overview to miniaturize the whole strip.
    pub fn placements_for_full_strip(&self, viewport: Rect) -> Vec<WindowPlacement> {
        let offsets = vec![0; self.rows.len()];
        self.compute_non_fullscreen_placements(viewport, &offsets)
    }

    /// Compute non-fullscreen placements for specific viewport-left offsets per row.
    /// Used by both static and animated placement paths.
    fn compute_non_fullscreen_placements(
        &self,
        viewport: Rect,
        viewport_lefts: &[i32],
    ) -> Vec<WindowPlacement> {
        let mut placements = Vec::new();
        let row_rects = self.row_rects(viewport);

        for (row_idx, row) in self.rows.iter().enumerate() {
            let row_rect = row_rects.get(row_idx).copied().unwrap_or(viewport);
            let viewport_left = viewport_lefts.get(row_idx).copied().unwrap_or(0);
            self.compute_row_placements(row_idx, row, row_rect, viewport_left, &mut placements);
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
                row_index: usize::MAX,
            });
        }

        placements
    }

    /// Compute placements for a single row within its allocated row rectangle.
    fn compute_row_placements(
        &self,
        row_idx: usize,
        row: &WorkspaceRow,
        row_rect: Rect,
        viewport_left: i32,
        placements: &mut Vec<WindowPlacement>,
    ) {
        let gap = self.gap.max(0);
        let outer_left = self.outer_gap_left.max(0);

        // Visible strip area inside viewport padding
        let vis_w = self.visible_width(row_rect.width);
        let visible_right = viewport_left.saturating_add(vis_w);

        // Strip starts at 0 — outer gaps are viewport padding
        let mut current_x: i32 = 0;

        for (col_idx, column) in row.columns.iter().enumerate() {
            let eff_width = self.effective_column_width(column);

            // Calculate column position in strip coordinates
            let col_strip_x = current_x;
            let col_strip_right = col_strip_x.saturating_add(eff_width);

            // Transform to screen coordinates:
            // strip_x → screen_x = strip_x - scroll_offset + row_rect.x + outer_left
            let natural_screen_x = col_strip_x - viewport_left + row_rect.x + outer_left;

            // Determine visibility against the visible strip area
            let visibility = if col_strip_right <= viewport_left {
                Visibility::OffScreenLeft
            } else if col_strip_x >= visible_right {
                Visibility::OffScreenRight
            } else {
                Visibility::Visible
            };

            let col_screen_x = match visibility {
                Visibility::OffScreenRight => {
                    natural_screen_x.max(row_rect.x.saturating_add(row_rect.width))
                }
                Visibility::OffScreenLeft => {
                    natural_screen_x.min(row_rect.x.saturating_sub(eff_width))
                }
                Visibility::Visible => natural_screen_x,
            };

            let visible_windows: Vec<(usize, WindowId)> = match column.mode() {
                crate::ColumnMode::Vertical => column
                    .windows()
                    .iter()
                    .enumerate()
                    .filter(|(_, w)| !self.minimized_windows.contains(w))
                    .map(|(i, &w)| (i, w))
                    .collect(),
                crate::ColumnMode::Tabbed { .. } => column
                    .effective_visible_tab(|w| self.minimized_windows.contains(&w))
                    .and_then(|i| column.windows().get(i).map(|&w| (i, w)))
                    .into_iter()
                    .collect(),
            };

            // Skip columns where all windows are minimized
            if !self.is_column_active(column) {
                continue;
            }

            // In Tabbed mode, off-screen tabs get an off-screen placement
            if column.is_tabbed() {
                let on_screen_idx = visible_windows.first().map(|(i, _)| *i);
                let offscreen_x = row_rect.x.saturating_sub(row_rect.width.max(1));
                for (i, &wid) in column.windows().iter().enumerate() {
                    if Some(i) == on_screen_idx {
                        continue;
                    }
                    if self.minimized_windows.contains(&wid) {
                        continue;
                    }
                    placements.push(WindowPlacement {
                        window_id: wid,
                        rect: Rect::new(offscreen_x, row_rect.y, 0, 0),
                        visibility: Visibility::OffScreenLeft,
                        column_index: col_idx,
                        row_index: row_idx,
                    });
                }
            }

            let column_top_reserve = if column.is_tabbed() {
                self.tab_strip_reserve_px.max(0)
            } else {
                0
            };

            let usable_height = row_rect
                .height
                .saturating_sub(column_top_reserve)
                .max(0);
            let window_count = visible_windows.len() as i32;
            let window_gaps = if window_count > 1 {
                gap.saturating_mul(window_count - 1)
            } else {
                0
            };
            let available_height = (usable_height - window_gaps).max(0);

            let visible_weights: Vec<f64> = if column.height_weights.len() == column.windows().len()
            {
                visible_windows
                    .iter()
                    .map(|(i, _)| column.height_weights[*i])
                    .collect()
            } else {
                vec![1.0; visible_windows.len()]
            };

            let min_heights: Vec<i32> = visible_windows
                .iter()
                .map(|(_, wid)| self.window_min_heights.get(wid).copied().unwrap_or(0))
                .collect();

            let pinned_sum: i32 = min_heights.iter().sum();
            let has_flex = min_heights.contains(&0);
            let flex_height = (available_height - pinned_sum).max(0);
            let flex_weight_sum: f64 = visible_windows
                .iter()
                .enumerate()
                .filter(|(idx, _)| min_heights[*idx] == 0)
                .map(|(idx, _)| visible_weights[idx])
                .sum::<f64>()
                .max(0.001);

            let mut current_y = row_rect.y + column_top_reserve;
            let mut allocated_height = 0;

            for (win_idx, &(_, window_id)) in visible_windows.iter().enumerate() {
                let is_last = win_idx == visible_windows.len() - 1;
                let height = if is_last {
                    (available_height - allocated_height).max(min_heights[win_idx])
                } else if min_heights[win_idx] > 0 {
                    min_heights[win_idx]
                } else if has_flex {
                    let share = visible_weights[win_idx] / flex_weight_sum;
                    (flex_height as f64 * share).round() as i32
                } else {
                    available_height / visible_windows.len().max(1) as i32
                };

                placements.push(WindowPlacement {
                    window_id,
                    rect: Rect::new(col_screen_x, current_y, eff_width, height),
                    visibility,
                    column_index: col_idx,
                    row_index: row_idx,
                });

                current_y = current_y.saturating_add(height).saturating_add(gap);
                allocated_height = allocated_height.saturating_add(height);
            }

            current_x = current_x.saturating_add(eff_width).saturating_add(gap);
        }
    }

    /// Compute placements for all windows, using animated scroll offset if active.
    ///
    /// This is similar to `compute_placements` but uses `effective_scroll_offset()`
    /// per row to support smooth scrolling animations.
    pub fn compute_placements_animated(&self, viewport: Rect) -> Vec<WindowPlacement> {
        let offsets: Vec<i32> = self
            .rows
            .iter()
            .map(|r| {
                match &r.active_animation {
                    Some(anim) => anim.current_offset().round() as i32,
                    None => r.scroll_offset.round() as i32,
                }
            })
            .collect();

        // Fullscreen mode: one window covers the entire viewport, others are off-screen
        if let Some(fs_wid) = self.fullscreen_window {
            return self.compute_fullscreen_placements(fs_wid, viewport, &offsets);
        }

        self.compute_non_fullscreen_placements(viewport, &offsets)
    }

    /// Compute placements when a window is fullscreen.
    ///
    /// The fullscreen window gets the full viewport; all others are marked
    /// off-screen but KEEP their real layout rects. Pinned floating windows stay
    /// Visible at their floating rect, above fullscreen.
    fn compute_fullscreen_placements(
        &self,
        fs_wid: WindowId,
        viewport: Rect,
        viewport_lefts: &[i32],
    ) -> Vec<WindowPlacement> {
        // Stale or minimized fullscreen target: fall back to normal placements.
        if !self.contains_window(fs_wid) || self.minimized_windows.contains(&fs_wid) {
            return self.compute_non_fullscreen_placements(viewport, viewport_lefts);
        }

        let mut placements = self.compute_non_fullscreen_placements(viewport, viewport_lefts);

        for placement in &mut placements {
            if placement.window_id == fs_wid {
                placement.rect = viewport;
                placement.visibility = Visibility::Visible;
            } else if placement.column_index == usize::MAX
                && self
                    .floating_windows
                    .iter()
                    .any(|f| f.id == placement.window_id && f.pinned)
            {
                // Pinned floating window: stays exactly as computed.
            } else {
                placement.visibility = Visibility::OffScreenLeft;
            }
        }

        placements
    }
}
