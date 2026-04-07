use crate::*;

use crate::workspace::Workspace;

impl Workspace {
    // ========================================================================
    // Minimum Width Methods
    // ========================================================================

    /// Record that a window enforces a minimum width (in layout pixels,
    /// i.e. excluding invisible border insets). The layout engine will
    /// respect this when computing column placements.
    pub fn set_window_min_width(&mut self, window_id: WindowId, min_width: i32) {
        self.window_min_widths.insert(window_id, min_width);
    }

    /// Remove a minimum-width constraint (e.g. when window is removed).
    pub fn clear_window_min_width(&mut self, window_id: WindowId) {
        self.window_min_widths.remove(&window_id);
    }

    /// Clear all minimum-width constraints. Called on display/theme changes
    /// because the border metrics used to compute them are no longer valid.
    pub fn clear_all_min_widths(&mut self) {
        self.window_min_widths.clear();
    }

    /// Get the effective minimum width for a column, considering all
    /// non-minimized windows in it that have known min-width constraints.
    pub(crate) fn column_effective_min_width(&self, column: &Column) -> i32 {
        column.windows().iter()
            .filter(|wid| !self.minimized_windows.contains(wid))
            .filter_map(|wid| self.window_min_widths.get(wid))
            .copied()
            .max()
            .unwrap_or(0)
    }

    // ========================================================================
    // Minimum Height Methods
    // ========================================================================

    /// Record that a window enforces a minimum height (in layout pixels).
    /// The layout engine will grant at least this much intra-column space
    /// to the window when computing placements.
    pub fn set_window_min_height(&mut self, window_id: WindowId, min_height: i32) {
        self.window_min_heights.insert(window_id, min_height);
    }

    /// Remove a minimum-height constraint (e.g. when a window is removed
    /// or a layout operation invalidates prior measurements).
    pub fn clear_window_min_height(&mut self, window_id: WindowId) {
        self.window_min_heights.remove(&window_id);
    }

    /// Clear all minimum-height constraints. Called on display/theme changes
    /// because the metrics used to compute them are no longer valid.
    pub fn clear_all_min_heights(&mut self) {
        self.window_min_heights.clear();
    }

    /// Adjust stored column widths to respect known min-width constraints.
    /// Columns containing min-width windows are widened to their minimum.
    /// Flexible columns keep their original widths — the total strip may grow,
    /// which is correct for a scroll-first tiling WM.  (Earlier versions
    /// shrunk flexible columns proportionally, but that caused cumulative
    /// narrowing on display/theme changes.)
    /// Returns `true` if any column was resized.
    pub fn apply_min_width_constraints(&mut self) -> bool {
        if self.window_min_widths.is_empty() {
            return false;
        }
        let mut changed = false;
        for col_idx in 0..self.columns.len() {
            if !self.is_column_active(&self.columns[col_idx]) {
                continue;
            }
            let min_w = self.column_effective_min_width(&self.columns[col_idx]);
            if min_w > self.columns[col_idx].width {
                self.columns[col_idx].width = min_w;
                changed = true;
            }
        }
        changed
    }

    // ========================================================================
    // Column Width Presets
    // ========================================================================

    /// Toggle maximize on the focused column.
    ///
    /// If currently maximized (and the sentinel window is still in the same column),
    /// restores the original width and returns `false`.
    /// Otherwise, saves the current width and expands the column to fill the
    /// visible viewport width, returning `true`.
    ///
    /// Exits fullscreen first if active (same as toggle_floating).
    pub fn toggle_maximize_column(&mut self, viewport_width: i32) -> bool {
        // Exit fullscreen first
        if let Some(fs_wid) = self.fullscreen_window.take() {
            self.window_min_widths.remove(&fs_wid);
            self.window_min_heights.remove(&fs_wid);
        }

        let vis_w = self.visible_width(viewport_width);

        // If already maximized, try to restore
        if let Some(state) = self.maximized_column.take() {
            // Find the column containing the sentinel window
            if let Some((col_idx, _)) = self.find_window_location(state.sentinel_window) {
                if let Some(column) = self.columns.get_mut(col_idx) {
                    column.set_width(state.original_width);
                }
            }
            return false;
        }

        // Maximize the focused column
        if let Some(column) = self.columns.get(self.focused_column) {
            let original_width = column.width;
            let sentinel_window = match column.windows().first() {
                Some(&wid) => wid,
                None => return false,
            };
            if let Some(column) = self.columns.get_mut(self.focused_column) {
                column.set_width(vis_w);
            }
            self.maximized_column = Some(super::MaximizedColumnState {
                original_width,
                sentinel_window,
            });
            return true;
        }

        false
    }

    /// Set the focused column's width as a fraction of the usable viewport width.
    /// The usable width accounts for outer gaps and inter-column gaps.
    /// Fraction should be between 0.1 and 1.0.
    pub fn set_focused_column_width_fraction(&mut self, fraction: f64, viewport_width: i32) {
        self.maximized_column = None;
        let fraction = fraction.clamp(0.1, 1.0);
        let base = self.width_base(viewport_width);
        let gap = self.gap.max(0);
        let new_width = (base as f64 * fraction - gap as f64).floor() as i32;

        if let Some(column) = self.columns.get_mut(self.focused_column) {
            column.set_width(new_width);
        }
    }

    /// Equalize all column widths to share the viewport equally.
    /// Uses gap-aware formula so equalized columns perfectly fill the viewport.
    /// Only counts active (non-fully-minimized) columns to match layout calculations.
    pub fn equalize_column_widths(&mut self, viewport_width: i32) {
        self.maximized_column = None;
        if self.columns.is_empty() {
            return;
        }
        // Clear cached min-widths and min-heights — equalize resets all widths,
        // so constraints will be re-detected from actual window sizes on the
        // next apply cycle.
        self.window_min_widths.clear();
        self.window_min_heights.clear();

        // Identify which columns are active (have at least one non-minimized window)
        let active_flags: Vec<bool> = self.columns.iter()
            .map(|c| self.is_column_active(c))
            .collect();
        let active_count = active_flags.iter().filter(|&&a| a).count() as i32;
        if active_count == 0 {
            return;
        }

        let outer_left = self.outer_gap_left.max(0);
        let outer_right = self.outer_gap_right.max(0);
        let gap = self.gap.max(0);
        let total_gaps = gap * (active_count - 1) + outer_left + outer_right;
        let per_column = ((viewport_width - total_gaps).max(MIN_COLUMN_WIDTH * active_count)) / active_count;

        for (col, &is_active) in self.columns.iter_mut().zip(active_flags.iter()) {
            if is_active {
                col.set_width(per_column);
            }
        }
        // Cancel stale animation — it would overwrite the reclamped scroll offset
        self.active_animation = None;
        // Reclamp scroll offset — column widths may have shrunk
        let vis_w = self.visible_width(viewport_width);
        let max_scroll = (self.total_width() - vis_w).max(0);
        self.scroll_offset = self.scroll_offset.clamp(0.0, max_scroll as f64);
    }

    /// Rescale all column widths after gap values change.
    /// Converts each column's current pixel width back to a fraction using the
    /// old gap values, then recomputes the pixel width with the current gaps.
    pub fn rescale_column_widths(
        &mut self,
        old_gap: i32,
        old_outer_left: i32,
        old_outer_right: i32,
        viewport_width: i32,
    ) {
        let old_gap_c = old_gap.max(0);
        let old_base = viewport_width
            .saturating_sub(old_outer_left.max(0))
            .saturating_sub(old_outer_right.max(0))
            .saturating_add(old_gap_c)
            .max(1);
        let new_base = self.width_base(viewport_width);
        let new_gap = self.gap.max(0);

        if old_base == new_base && old_gap_c == new_gap {
            return;
        }

        for col in &mut self.columns {
            let frac = (col.width + old_gap_c) as f64 / old_base as f64;
            let new_width = (new_base as f64 * frac - new_gap as f64).round() as i32;
            col.set_width(new_width);
        }
    }

    // ========================================================================
    // Width Preset Cycling
    // ========================================================================

    /// Compute the base value for fraction ↔ pixel conversion.
    /// Formula: `column_width = fraction * base - gap`.
    /// This is independent of column count. When fractions sum to 1.0,
    /// the columns plus gaps perfectly fill the viewport.
    fn width_base(&self, viewport_width: i32) -> i32 {
        let outer_left = self.outer_gap_left.max(0);
        let outer_right = self.outer_gap_right.max(0);
        let gap = self.gap.max(0);
        viewport_width
            .saturating_sub(outer_left)
            .saturating_sub(outer_right)
            .saturating_add(gap)
            .max(0)
    }

    /// Cycle the focused column width up through the given presets.
    pub fn cycle_width_up(&mut self, presets: &[f64], viewport_width: i32) {
        self.maximized_column = None;
        if presets.is_empty() {
            return;
        }
        let base = self.width_base(viewport_width);
        let gap = self.gap.max(0);
        let Some(column) = self.columns.get_mut(self.focused_column) else {
            return;
        };
        if base <= 0 {
            return;
        }
        let current_frac = (column.width + gap) as f64 / base as f64;

        let mut sorted = presets.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        const TOLERANCE: f64 = 0.005;
        let target = sorted.iter().find(|&&p| p > current_frac + TOLERANCE);
        if let Some(&frac) = target {
            let new_width = (base as f64 * frac - gap as f64).floor() as i32;
            column.set_width(new_width);
        }
    }

    /// Cycle the focused column width down through the given presets.
    pub fn cycle_width_down(&mut self, presets: &[f64], viewport_width: i32) {
        self.maximized_column = None;
        if presets.is_empty() {
            return;
        }
        let base = self.width_base(viewport_width);
        let gap = self.gap.max(0);
        let Some(column) = self.columns.get_mut(self.focused_column) else {
            return;
        };
        if base <= 0 {
            return;
        }
        let current_frac = (column.width + gap) as f64 / base as f64;

        let mut sorted = presets.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        const TOLERANCE: f64 = 0.005;
        let target = sorted.iter().rev().find(|&&p| p < current_frac - TOLERANCE);
        if let Some(&frac) = target {
            let new_width = (base as f64 * frac - gap as f64).floor() as i32;
            column.set_width(new_width);
        }
    }

    /// Snap a column's width to the nearest width preset based on the new pixel width
    /// from a user resize. Respects min-width constraints: if the nearest preset is
    /// narrower than the column's minimum, the smallest valid preset is used instead.
    pub fn snap_column_width_to_preset(
        &mut self,
        col_idx: usize,
        new_width: i32,
        presets: &[f64],
        viewport_width: i32,
    ) {
        if presets.is_empty() {
            return;
        }
        let Some(column) = self.columns.get(col_idx) else {
            return;
        };

        let base = self.width_base(viewport_width);
        let gap = self.gap.max(0);
        if base <= 0 {
            return;
        }

        // Fraction corresponding to the user's resized width
        let current_frac = (new_width + gap) as f64 / base as f64;

        // Min-width constraint for this column
        let min_width = self.column_effective_min_width(column);
        let min_frac = (min_width + gap) as f64 / base as f64;

        let mut sorted = presets.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        // Find closest preset
        let nearest = sorted
            .iter()
            .min_by(|&&a, &&b| {
                let da = (a - current_frac).abs();
                let db = (b - current_frac).abs();
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .copied();

        if let Some(frac) = nearest {
            // If nearest is below minimum width, use smallest valid preset
            let final_frac = if frac < min_frac {
                sorted
                    .iter()
                    .find(|&&p| p >= min_frac)
                    .copied()
                    .unwrap_or(frac)
            } else {
                frac
            };

            let new_w = (base as f64 * final_frac - gap as f64).floor() as i32;
            if let Some(column) = self.columns.get_mut(col_idx) {
                column.set_width(new_w);
            }
            // Clear cached min-widths and min-heights so constraints are re-detected
            if let Some(column) = self.columns.get(col_idx) {
                for wid in column.windows() {
                    self.window_min_widths.remove(wid);
                    self.window_min_heights.remove(wid);
                }
            }
        }
    }

    /// Snap a window's height weight to the nearest height preset based on the
    /// new pixel height from a user resize. Only meaningful for multi-window columns.
    pub fn snap_window_height_to_preset(
        &mut self,
        col_idx: usize,
        win_idx: usize,
        new_height: i32,
        presets: &[f64],
        viewport_height: i32,
    ) {
        if presets.is_empty() {
            return;
        }
        let Some(column) = self.columns.get(col_idx) else {
            return;
        };
        if column.len() <= 1 {
            return;
        }

        // Compute available height (viewport minus outer gaps and window gaps)
        let outer_top = self.outer_gap_top.max(0);
        let outer_bottom = self.outer_gap_bottom.max(0);
        let gap = self.gap.max(0);
        let window_gaps = gap.saturating_mul(column.len() as i32 - 1);
        let available_height =
            (viewport_height - outer_top - outer_bottom - window_gaps).max(1);

        // Weight corresponding to the user's resized height
        let current_weight = new_height as f64 / available_height as f64;

        let mut sorted = presets.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let nearest = sorted
            .iter()
            .min_by(|&&a, &&b| {
                let da = (a - current_weight).abs();
                let db = (b - current_weight).abs();
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .copied();

        if let Some(weight) = nearest {
            if let Some(column) = self.columns.get_mut(col_idx) {
                column.set_height_weight(win_idx, weight);
            }
        }
    }

    // ========================================================================
    // Height Preset Cycling
    // ========================================================================

    /// Cycle the focused window's height weight up through the given presets.
    /// Presets are fractions of column height (weight values).
    /// No-op for single-window columns.
    pub fn cycle_height_up(&mut self, presets: &[f64]) {
        self.cycle_height_impl(presets, true);
    }

    /// Cycle the focused window's height weight down through the given presets.
    /// No-op for single-window columns.
    pub fn cycle_height_down(&mut self, presets: &[f64]) {
        self.cycle_height_impl(presets, false);
    }

    fn cycle_height_impl(&mut self, presets: &[f64], up: bool) {
        if presets.is_empty() {
            return;
        }
        let col_idx = self.focused_column;
        let win_idx = self.focused_window_in_column;
        let col = match self.columns.get_mut(col_idx) {
            Some(c) => c,
            None => return,
        };
        if col.len() <= 1 {
            return;
        }
        col.ensure_height_weights();
        let current_weight = col.height_weights[win_idx];

        let mut sorted = presets.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        const TOLERANCE: f64 = 0.005;
        let target = if up {
            sorted.iter().find(|&&p| p > current_weight + TOLERANCE).copied()
        } else {
            sorted.iter().rev().find(|&&p| p < current_weight - TOLERANCE).copied()
        };

        if let Some(frac) = target {
            col.set_height_weight(win_idx, frac);
        }
    }

    /// Equalize height weights in the focused column.
    pub fn equalize_focused_column_heights(&mut self) {
        if let Some(col) = self.columns.get_mut(self.focused_column) {
            col.equalize_height_weights();
        }
    }

    /// Set scroll offset directly (bypasses clamping).
    pub fn set_scroll_offset(&mut self, offset: f64) {
        self.scroll_offset = offset;
    }

    /// Set all column widths to a uniform value.
    pub fn set_all_column_widths(&mut self, width: i32) {
        for col in &mut self.columns {
            col.set_width(width);
        }
    }

    // ========================================================================
    // Resize Preview
    // ========================================================================

    /// Compute the nearest width preset in pixels for a given column width,
    /// without mutating any workspace state.
    fn nearest_preset_width(
        &self,
        col_idx: usize,
        current_width: i32,
        presets: &[f64],
        viewport_width: i32,
    ) -> Option<i32> {
        if presets.is_empty() {
            return None;
        }
        let column = self.columns.get(col_idx)?;
        let base = self.width_base(viewport_width);
        let gap = self.gap.max(0);
        if base <= 0 {
            return None;
        }

        let current_frac = (current_width + gap) as f64 / base as f64;
        let min_width = self.column_effective_min_width(column);
        let min_frac = (min_width + gap) as f64 / base as f64;

        let mut sorted = presets.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let nearest = sorted
            .iter()
            .min_by(|&&a, &&b| {
                let da = (a - current_frac).abs();
                let db = (b - current_frac).abs();
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .copied()?;

        let final_frac = if nearest < min_frac {
            sorted
                .iter()
                .find(|&&p| p >= min_frac)
                .copied()
                .unwrap_or(nearest)
        } else {
            nearest
        };

        Some((base as f64 * final_frac - gap as f64).floor() as i32)
    }

    /// Compute the nearest height weight preset for a window, without mutating state.
    fn nearest_preset_height_weight(
        &self,
        col_idx: usize,
        current_height: i32,
        presets: &[f64],
        viewport_height: i32,
    ) -> Option<f64> {
        if presets.is_empty() {
            return None;
        }
        let column = self.columns.get(col_idx)?;
        if column.len() <= 1 {
            return None;
        }

        let outer_top = self.outer_gap_top.max(0);
        let outer_bottom = self.outer_gap_bottom.max(0);
        let gap = self.gap.max(0);
        let window_gaps = gap.saturating_mul(column.len() as i32 - 1);
        let available_height =
            (viewport_height - outer_top - outer_bottom - window_gaps).max(1);

        let current_weight = current_height as f64 / available_height as f64;

        let mut sorted = presets.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        sorted
            .iter()
            .min_by(|&&a, &&b| {
                let da = (a - current_weight).abs();
                let db = (b - current_weight).abs();
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .copied()
    }

    /// Compute the placement rect a window would occupy after snapping its
    /// column width and height to the nearest presets. Used for resize preview
    /// ghost overlay. Temporarily mutates column state and restores it.
    pub fn preview_resize_snap(
        &mut self,
        window_id: WindowId,
        current_width: i32,
        current_height: i32,
        width_presets: &[f64],
        height_presets: &[f64],
        viewport: Rect,
    ) -> Option<Rect> {
        let (col_idx, win_idx) = self.find_window_location(window_id)?;

        // Compute snapped values (read-only)
        let snapped_width =
            self.nearest_preset_width(col_idx, current_width, width_presets, viewport.width);
        let snapped_weight = self.nearest_preset_height_weight(
            col_idx,
            current_height,
            height_presets,
            viewport.height,
        );

        // Save originals
        let original_width = self.columns[col_idx].width;
        let original_weights = self.columns[col_idx].height_weights.clone();

        // Temporarily apply snapped values
        if let Some(w) = snapped_width {
            self.columns[col_idx].set_width(w);
        }
        if let Some(weight) = snapped_weight {
            self.columns[col_idx].set_height_weight(win_idx, weight);
        }

        // Compute placements with snapped values
        let placements = self.compute_placements(viewport);
        let rect = placements
            .iter()
            .find(|p| p.window_id == window_id)
            .map(|p| p.rect);

        // Restore originals
        self.columns[col_idx].width = original_width;
        self.columns[col_idx].height_weights = original_weights;

        rect
    }
}
