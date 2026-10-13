use crate::*;

use crate::workspace::Workspace;

enum PresetCycle {
    Up,
    Down,
    Wrap,
}

fn cycle_preset(presets: &[f64], current: f64, cycle: PresetCycle) -> Option<f64> {
    let mut sorted = presets.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    const TOLERANCE: f64 = 0.005;
    let target = match cycle {
        PresetCycle::Down => sorted
            .iter()
            .rev()
            .find(|&&p| p < current - TOLERANCE)
            .copied(),
        PresetCycle::Up | PresetCycle::Wrap => {
            sorted.iter().find(|&&p| p > current + TOLERANCE).copied()
        }
    };
    target.or_else(|| match cycle {
        PresetCycle::Wrap => sorted.first().copied(),
        _ => None,
    })
}

impl Workspace {
    // ========================================================================
    // Minimum Width Methods
    // ========================================================================

    /// Record that a window enforces a minimum width (in layout pixels).
    pub fn set_window_min_width(&mut self, window_id: WindowId, min_width: i32) {
        self.window_min_widths.insert(window_id, min_width);
    }

    /// Remove a minimum-width constraint.
    pub fn clear_window_min_width(&mut self, window_id: WindowId) {
        self.window_min_widths.remove(&window_id);
    }

    /// Clear all minimum-width constraints.
    pub fn clear_all_min_widths(&mut self) {
        self.window_min_widths.clear();
    }

    /// Get the effective minimum width for a column.
    pub(crate) fn column_effective_min_width(&self, column: &Column) -> i32 {
        column
            .windows()
            .iter()
            .filter(|wid| !self.minimized_windows.contains(wid))
            .filter_map(|wid| self.window_min_widths.get(wid))
            .copied()
            .max()
            .unwrap_or(0)
    }

    /// Native minima affect strip geometry, never the requested/persisted width.
    pub fn effective_column_width(&self, column: &Column) -> i32 {
        column.width.max(self.column_effective_min_width(column))
    }

    // ========================================================================
    // Minimum Height Methods
    // ========================================================================

    /// Record that a window enforces a minimum height (in layout pixels).
    pub fn set_window_min_height(&mut self, window_id: WindowId, min_height: i32) {
        self.window_min_heights.insert(window_id, min_height);
    }

    /// Remove a minimum-height constraint.
    pub fn clear_window_min_height(&mut self, window_id: WindowId) {
        self.window_min_heights.remove(&window_id);
    }

    /// Clear all minimum-height constraints.
    pub fn clear_all_min_heights(&mut self) {
        self.window_min_heights.clear();
    }

    /// Get the recorded minimum height for a window, if any.
    pub fn window_min_height(&self, window_id: WindowId) -> Option<i32> {
        self.window_min_heights.get(&window_id).copied()
    }

    /// Take and clear the set of windows whose min-size constraints should be cleared.
    pub fn take_pending_min_size_clears(&mut self) -> std::collections::HashSet<WindowId> {
        std::mem::take(&mut self.pending_min_size_clears)
    }

    /// Clear cached min-sizes for the given windows.
    pub fn clear_min_sizes_for(&mut self, windows: &std::collections::HashSet<WindowId>) {
        for wid in windows {
            self.window_min_widths.remove(wid);
            self.window_min_heights.remove(wid);
        }
    }

    // ========================================================================
    // Column Width Sizing Methods
    // ========================================================================

    /// Set a column's width in the focused row directly, clamping to minimum width.
    pub fn set_column_width(&mut self, column_index: usize, width: i32) -> Result<(), LayoutError> {
        let fr = self.focused_row;
        let row = &mut self.rows[fr];
        row.maximized_column = None;
        if column_index >= row.columns.len() {
            return Err(LayoutError::ColumnOutOfBounds(
                column_index,
                row.columns.len().saturating_sub(1),
            ));
        }
        row.columns[column_index].set_width(width);
        for wid in row.columns[column_index].windows() {
            self.window_min_widths.remove(wid);
            self.window_min_heights.remove(wid);
        }
        Ok(())
    }

    /// Drain the pending_min_size_clears queue and remove corresponding entries
    /// from window_min_widths / window_min_heights.
    pub fn commit_pending_min_size_clears(&mut self) -> bool {
        if self.pending_min_size_clears.is_empty() {
            return false;
        }
        for wid in self.pending_min_size_clears.drain() {
            self.window_min_widths.remove(&wid);
            self.window_min_heights.remove(&wid);
        }
        true
    }

    /// Maximize the focused column unconditionally (restoring any other
    /// maximized column first). Unlike the toggle, this never un-maximizes
    /// the focused column; used by per-app open rules.
    pub fn maximize_focused_column(&mut self, viewport_width: i32) {
        if self.rows[self.focused_row].maximized_column.is_some() {
            self.toggle_maximize_column(viewport_width);
        }
        self.toggle_maximize_column(viewport_width);
    }

    /// Alias for toggle_maximized_column.
    pub fn toggle_maximize_column(&mut self, viewport_width: i32) -> bool {
        self.toggle_maximized_column(viewport_width)
    }

    /// Toggle maximizing the focused column to fill the viewport width.
    pub fn toggle_maximized_column(&mut self, viewport_width: i32) -> bool {
        let fr = self.focused_row;
        let vis_w = self.visible_width(viewport_width);

        if let Some(state) = self.rows[fr].maximized_column.take() {
            if let Some((col_idx, _)) = self.find_window_location(state.sentinel_window) {
                if let Some(column) = self.rows[fr].columns.get_mut(col_idx) {
                    column.set_width(state.original_width);
                    column.width_fraction_cache = state.width_fraction_cache;
                }
            }
            return false;
        }

        let row = &mut self.rows[fr];
        if let Some(column) = row.columns.get(row.focused_column) {
            let original_width = column.width;
            let sentinel_window = match column.windows().first() {
                Some(&wid) => wid,
                None => return false,
            };
            let width_fraction_cache = column.width_fraction_cache.clone();
            if let Some(column) = row.columns.get_mut(row.focused_column) {
                column.set_width(vis_w);
            }
            row.maximized_column = Some(super::MaximizedColumnState {
                original_width,
                width_fraction_cache,
                sentinel_window,
            });
            return true;
        }

        false
    }

    /// Set the focused column's width as a fraction of usable viewport width.
    pub fn set_focused_column_width_fraction(&mut self, fraction: f64, viewport_width: i32) {
        let fr = self.focused_row;
        self.rows[fr].maximized_column = None;
        let fraction = fraction.clamp(0.1, 1.0);
        let base = self.width_base(viewport_width);
        let gap = self.gap.max(0);
        let new_width = (base as f64 * fraction - gap as f64).floor() as i32;

        let row = &mut self.rows[fr];
        if let Some(column) = row.columns.get_mut(row.focused_column) {
            column.set_width(new_width);
        }
    }

    /// Equalize all column widths in the focused row to share the viewport equally.
    pub fn equalize_column_widths(&mut self, viewport_width: i32) {
        let fr = self.focused_row;
        self.rows[fr].maximized_column = None;
        if self.rows[fr].columns.is_empty() {
            return;
        }
        self.window_min_widths.clear();
        self.window_min_heights.clear();

        let active_flags: Vec<bool> = self.rows[fr]
            .columns
            .iter()
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
        let per_column =
            ((viewport_width - total_gaps).max(MIN_COLUMN_WIDTH * active_count)) / active_count;

        for (col, &is_active) in self.rows[fr].columns.iter_mut().zip(active_flags.iter()) {
            if is_active {
                col.set_width(per_column);
            }
        }
        self.rows[fr].active_animation = None;
        let vis_w = self.visible_width(viewport_width);
        let max_scroll = (self.total_width_for_row(fr) - vis_w).max(0);
        self.rows[fr].scroll_offset = self.clamp_scroll_offset_for_row(
            fr,
            self.rows[fr].scroll_offset,
            viewport_width,
            0.0,
            max_scroll as f64,
        );
    }

    /// Rescale all column widths after viewport or gap values change.
    pub fn rescale_column_widths(
        &mut self,
        old_gap: i32,
        old_outer_left: i32,
        old_outer_right: i32,
        old_viewport_width: i32,
        new_viewport_width: i32,
    ) -> bool {
        let old_gap_c = old_gap.max(0);
        let old_base = old_viewport_width
            .saturating_sub(old_outer_left.max(0))
            .saturating_sub(old_outer_right.max(0))
            .saturating_add(old_gap_c)
            .max(1);
        let new_base = self.width_base(new_viewport_width);
        let new_gap = self.gap.max(0);

        if old_base == new_base && old_gap_c == new_gap {
            return false;
        }

        self.cancel_animation();

        for row in &mut self.rows {
            for col in &mut row.columns {
                let width = Self::rescaled_width(
                    col.width,
                    &mut col.width_fraction_cache,
                    old_gap_c,
                    old_base,
                    new_gap,
                    new_base,
                );
                col.set_width(width);
            }
            if let Some(state) = &mut row.maximized_column {
                state.original_width = Self::rescaled_width(
                    state.original_width,
                    &mut state.width_fraction_cache,
                    old_gap_c,
                    old_base,
                    new_gap,
                    new_base,
                );
            }
        }

        let vis_w = self.visible_width(new_viewport_width);
        for row_idx in 0..self.rows.len() {
            let max_scroll = (self.total_width_for_row(row_idx) - vis_w).max(0) as f64;
            let min_scroll = if self.center_past_edges {
                f64::NEG_INFINITY
            } else {
                0.0
            };
            self.rows[row_idx].scroll_offset = self.clamp_scroll_offset_for_row(
                row_idx,
                self.rows[row_idx].scroll_offset,
                new_viewport_width,
                min_scroll,
                max_scroll,
            );
        }

        true
    }

    fn rescaled_width(
        width: i32,
        cache: &mut Option<crate::column::WidthFractionCache>,
        old_gap: i32,
        old_base: i32,
        new_gap: i32,
        new_base: i32,
    ) -> i32 {
        let fraction = match cache.as_ref() {
            Some(cached)
                if cached.width == width && cached.base == old_base && cached.gap == old_gap =>
            {
                cached.fraction
            }
            _ => width.saturating_add(old_gap) as f64 / old_base as f64,
        };
        let width = (new_base as f64 * fraction - new_gap as f64)
            .round()
            .clamp(MIN_COLUMN_WIDTH as f64, i32::MAX as f64) as i32;
        *cache = Some(crate::column::WidthFractionCache {
            fraction,
            width,
            base: new_base,
            gap: new_gap,
        });
        width
    }

    // ========================================================================
    // Width Preset Cycling
    // ========================================================================

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

    /// Cycle the focused column width through presets, wrapping to the smallest.
    pub fn cycle_width(&mut self, presets: &[f64], viewport_width: i32) {
        self.cycle_width_impl(presets, viewport_width, PresetCycle::Wrap);
    }

    /// Cycle the focused column width up through the given presets.
    pub fn cycle_width_up(&mut self, presets: &[f64], viewport_width: i32) {
        self.cycle_width_impl(presets, viewport_width, PresetCycle::Up);
    }

    pub fn cycle_width_down(&mut self, presets: &[f64], viewport_width: i32) {
        self.cycle_width_impl(presets, viewport_width, PresetCycle::Down);
    }

    fn cycle_width_impl(&mut self, presets: &[f64], viewport_width: i32, cycle: PresetCycle) {
        let fr = self.focused_row;
        self.rows[fr].maximized_column = None;
        if presets.is_empty() {
            return;
        }
        let base = self.width_base(viewport_width);
        let gap = self.gap.max(0);
        let foc = self.rows[fr].focused_column;
        let Some(column) = self.rows[fr].columns.get(foc) else {
            return;
        };
        if base <= 0 {
            return;
        }
        let current_frac =
            self.effective_column_width(column).saturating_add(gap) as f64 / base as f64;

        if let Some(frac) = cycle_preset(presets, current_frac, cycle) {
            let new_width = (base as f64 * frac - gap as f64).floor() as i32;
            self.rows[fr].columns[foc].set_width(new_width);
        }
    }

    pub fn snap_column_width_to_preset(
        &mut self,
        col_idx: usize,
        new_width: i32,
        presets: &[f64],
        viewport_width: i32,
    ) {
        let fr = self.focused_row;
        if presets.is_empty() {
            return;
        }
        let Some(column) = self.rows[fr].columns.get(col_idx) else {
            return;
        };

        let base = self.width_base(viewport_width);
        let gap = self.gap.max(0);
        if base <= 0 {
            return;
        }

        let current_frac = (new_width + gap) as f64 / base as f64;
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
            .copied();

        if let Some(frac) = nearest {
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
            if let Some(column) = self.rows[fr].columns.get_mut(col_idx) {
                column.set_width(new_w);
            }
            if let Some(column) = self.rows[fr].columns.get(col_idx) {
                for wid in column.windows() {
                    self.window_min_widths.remove(wid);
                    self.window_min_heights.remove(wid);
                }
            }
        }
    }

    pub fn snap_window_height_to_preset(
        &mut self,
        col_idx: usize,
        win_idx: usize,
        new_height: i32,
        presets: &[f64],
        viewport_height: i32,
    ) {
        let fr = self.focused_row;
        if presets.is_empty() {
            return;
        }
        let Some(column) = self.rows[fr].columns.get(col_idx) else {
            return;
        };
        if column.len() <= 1 || column.is_tabbed() {
            return;
        }

        let outer_top = self.outer_gap_top.max(0);
        let outer_bottom = self.outer_gap_bottom.max(0);
        let gap = self.gap.max(0);
        let window_gaps = gap.saturating_mul(column.len() as i32 - 1);
        let available_height = (viewport_height - outer_top - outer_bottom - window_gaps).max(1);

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
            if let Some(column) = self.rows[fr].columns.get_mut(col_idx) {
                column.set_height_weight(win_idx, weight);
            }
        }
    }

    // ========================================================================
    // Height Preset Cycling
    // ========================================================================

    /// Cycle the focused window's height through presets, wrapping to the smallest.
    /// No-op for single-window or tabbed columns.
    pub fn cycle_height(&mut self, presets: &[f64]) {
        self.cycle_height_impl(presets, PresetCycle::Wrap);
    }

    /// Cycle the focused window's height weight up through the given presets.
    /// Presets are fractions of column height (weight values).
    /// No-op for single-window columns.
    pub fn cycle_height_up(&mut self, presets: &[f64]) {
        self.cycle_height_impl(presets, PresetCycle::Up);
    }

    pub fn cycle_height_down(&mut self, presets: &[f64]) {
        self.cycle_height_impl(presets, PresetCycle::Down);
    }

    fn cycle_height_impl(&mut self, presets: &[f64], cycle: PresetCycle) {
        if presets.is_empty() {
            return;
        }
        let fr = self.focused_row;
        let col_idx = self.rows[fr].focused_column;
        let win_idx = self.rows[fr].focused_window_in_column;
        let col = match self.rows[fr].columns.get_mut(col_idx) {
            Some(c) => c,
            None => return,
        };
        if col.len() <= 1 || col.is_tabbed() {
            return;
        }
        col.ensure_height_weights();
        let current_weight = col.height_weights[win_idx];

        let wrapping = matches!(cycle, PresetCycle::Wrap);
        if let Some(frac) = cycle_preset(presets, current_weight, cycle) {
            col.set_height_weight(win_idx, frac);
            if wrapping && frac > current_weight && col.height_weights[win_idx] <= current_weight {
                // A larger preset can clamp back to the sibling ceiling instead of advancing.
                if let Some(smallest) = cycle_preset(presets, f64::INFINITY, PresetCycle::Wrap) {
                    col.set_height_weight(win_idx, smallest);
                }
            }
        }
    }

    pub fn equalize_focused_column_heights(&mut self) {
        let fr = self.focused_row;
        let foc = self.rows[fr].focused_column;
        if let Some(col) = self.rows[fr].columns.get_mut(foc) {
            if col.is_tabbed() {
                return;
            }
            col.equalize_height_weights();
        }
    }

    /// Set all column widths to a uniform value across all rows.
    pub fn set_all_column_widths(&mut self, width: i32) {
        for row in &mut self.rows {
            for col in &mut row.columns {
                col.set_width(width);
            }
        }
    }

    // ========================================================================
    // Resize Preview
    // ========================================================================

    fn nearest_preset_width_in_row(
        &self,
        row_idx: usize,
        col_idx: usize,
        current_width: i32,
        presets: &[f64],
        viewport_width: i32,
    ) -> Option<i32> {
        if presets.is_empty() {
            return None;
        }
        let column = self.rows.get(row_idx)?.columns.get(col_idx)?;
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

    fn nearest_preset_height_weight_in_row(
        &self,
        row_idx: usize,
        col_idx: usize,
        current_height: i32,
        presets: &[f64],
        row_height: i32,
    ) -> Option<f64> {
        if presets.is_empty() {
            return None;
        }
        let column = self.rows.get(row_idx)?.columns.get(col_idx)?;
        if column.len() <= 1 || column.is_tabbed() {
            return None;
        }

        let outer_top = if row_idx == 0 { self.outer_gap_top.max(0) } else { 0 };
        let outer_bottom = if row_idx == self.rows.len() - 1 { self.outer_gap_bottom.max(0) } else { 0 };
        let gap = self.gap.max(0);
        let window_gaps = gap.saturating_mul(column.len() as i32 - 1);
        let available_height = (row_height - outer_top - outer_bottom - window_gaps).max(1);

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
    /// column width and height to the nearest presets.
    pub fn preview_resize_snap(
        &mut self,
        window_id: WindowId,
        current_width: i32,
        current_height: i32,
        width_presets: &[f64],
        height_presets: &[f64],
        viewport: Rect,
    ) -> Option<Rect> {
        let (row_idx, col_idx, win_idx) = self.find_window_location_rc(window_id)?;

        let row_rects = self.row_rects(viewport);
        let row_h = row_rects.get(row_idx).map(|r| r.height).unwrap_or(viewport.height);

        let snapped_width = self.nearest_preset_width_in_row(
            row_idx,
            col_idx,
            current_width,
            width_presets,
            viewport.width,
        );
        let snapped_weight = self.nearest_preset_height_weight_in_row(
            row_idx,
            col_idx,
            current_height,
            height_presets,
            row_h,
        );

        let original_width = self.rows[row_idx].columns[col_idx].width;
        let original_weights = self.rows[row_idx].columns[col_idx].height_weights.clone();

        if let Some(w) = snapped_width {
            self.rows[row_idx].columns[col_idx].set_width(w);
        }
        if let Some(weight) = snapped_weight {
            self.rows[row_idx].columns[col_idx].set_height_weight(win_idx, weight);
        }

        let placements = self.compute_placements(viewport);
        let rect = placements
            .iter()
            .find(|p| p.window_id == window_id)
            .map(|p| p.rect);

        self.rows[row_idx].columns[col_idx].width = original_width;
        self.rows[row_idx].columns[col_idx].height_weights = original_weights;

        rect
    }
}
