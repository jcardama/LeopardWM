use crate::*;

use crate::workspace::Workspace;

/// Direction and wrapping behavior for preset cycling.
pub enum PresetCycle {
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
    /// Cycle a floating dimension through the same presets as tiled windows.
    pub fn cycle_floating_size(
        &mut self,
        window_id: WindowId,
        area: Rect,
        presets: &[f64],
        horizontal: bool,
        cycle: PresetCycle,
    ) {
        let Some(window) = self
            .floating_windows
            .iter()
            .find(|window| window.id == window_id)
        else {
            return;
        };
        let (current, base) = if horizontal {
            (window.rect.width, area.width)
        } else {
            (window.rect.height, area.height)
        };
        if base <= 0 {
            return;
        }
        if let Some(fraction) = cycle_preset(presets, current as f64 / base as f64, cycle) {
            self.set_floating_size(
                window_id,
                (base as f64 * fraction).floor() as i32,
                horizontal,
                area,
            );
        } else {
            self.move_floating(window_id, 0, 0, area);
        }
    }

    /// Records a placement minimum width in layout pixels, excluding invisible border insets.
    pub fn set_window_min_width(&mut self, window_id: WindowId, min_width: i32) {
        self.window_min_widths.insert(window_id, min_width);
    }

    pub fn clear_window_min_width(&mut self, window_id: WindowId) {
        self.window_min_widths.remove(&window_id);
    }

    /// Clears measured widths invalidated by display or theme metric changes.
    pub fn clear_all_min_widths(&mut self) {
        self.window_min_widths.clear();
    }

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

    /// Records the minimum placement height in layout pixels.
    pub fn set_window_min_height(&mut self, window_id: WindowId, min_height: i32) {
        self.window_min_heights.insert(window_id, min_height);
    }

    pub fn clear_window_min_height(&mut self, window_id: WindowId) {
        self.window_min_heights.remove(&window_id);
    }

    /// Clears measured heights invalidated by display or theme metric changes.
    pub fn clear_all_min_heights(&mut self) {
        self.window_min_heights.clear();
    }

    /// Applies queued constraint clears; call only when a layout apply can run.
    /// Paused or timed-out applies must retain constraints. Returns whether the queue was nonempty.
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

    /// Maximizes the focused column after restoring any existing maximized column.
    pub fn maximize_focused_column(&mut self, viewport_width: i32) {
        if self.maximized_column.is_some() {
            self.toggle_maximize_column(viewport_width);
        }
        self.toggle_maximize_column(viewport_width);
    }

    /// With saved maximize state, restores the sentinel's column if found and returns `false`.
    /// Otherwise maximizes focus and returns `true`; no valid column returns `false`.
    /// Exits fullscreen first.
    pub fn toggle_maximize_column(&mut self, viewport_width: i32) -> bool {
        if let Some(fs_wid) = self.fullscreen_window.take() {
            self.window_min_widths.remove(&fs_wid);
            self.window_min_heights.remove(&fs_wid);
        }

        let vis_w = self.visible_width(viewport_width);

        if let Some(state) = self.maximized_column.take() {
            if let Some((col_idx, _)) = self.find_window_location(state.sentinel_window) {
                if let Some(column) = self.columns.get_mut(col_idx) {
                    column.set_width(state.original_width);
                    column.width_fraction_cache = state.width_fraction_cache;
                }
            }
            return false;
        }

        if let Some(column) = self.columns.get(self.focused_column) {
            let original_width = column.width;
            let sentinel_window = match column.windows().first() {
                Some(&wid) => wid,
                None => return false,
            };
            let width_fraction_cache = column.width_fraction_cache.clone();
            if let Some(column) = self.columns.get_mut(self.focused_column) {
                column.set_width(vis_w);
            }
            self.maximized_column = Some(super::MaximizedColumnState {
                original_width,
                width_fraction_cache,
                sentinel_window,
            });
            return true;
        }

        false
    }

    /// Sets width using the gap-aware viewport fraction, clamped to 0.1–1.0.
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

    /// Shares viewport width among non-fully-minimized columns, accounting for gaps and the minimum
    /// column width.
    pub fn equalize_column_widths(&mut self, viewport_width: i32) {
        self.maximized_column = None;
        if self.columns.is_empty() {
            return;
        }
        // Width changes invalidate measured minima; the next apply re-detects them.
        self.window_min_widths.clear();
        self.window_min_heights.clear();

        let active_flags: Vec<bool> = self
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

        for (col, &is_active) in self.columns.iter_mut().zip(active_flags.iter()) {
            if is_active {
                col.set_width(per_column);
            }
        }
        // Cancel stale animation — it would overwrite the reclamped scroll offset
        self.active_animation = None;
        let vis_w = self.visible_width(viewport_width);
        let max_scroll = (self.total_width() - vis_w).max(0);
        self.scroll_offset =
            self.clamp_scroll_offset(self.scroll_offset, viewport_width, 0.0, max_scroll as f64);
    }

    /// Rescale all column widths after viewport or gap values change.
    /// Reuses the previous rescale's fraction while width and geometry are unchanged;
    /// otherwise derives it from the current pixel width and old geometry.
    /// Returns `true` when the effective geometry changed.
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

        for col in &mut self.columns {
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
        if let Some(state) = &mut self.maximized_column {
            state.original_width = Self::rescaled_width(
                state.original_width,
                &mut state.width_fraction_cache,
                old_gap_c,
                old_base,
                new_gap,
                new_base,
            );
        }

        let vis_w = self.visible_width(new_viewport_width);
        let max_scroll = (self.total_width() - vis_w).max(0) as f64;
        let min_scroll = if self.center_past_edges {
            f64::NEG_INFINITY
        } else {
            0.0
        };
        self.scroll_offset = self.clamp_scroll_offset(
            self.scroll_offset,
            new_viewport_width,
            min_scroll,
            max_scroll,
        );

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

    // Fractions use column_width = fraction * base - gap so widths and gaps share the viewport.
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

    pub fn cycle_width_up(&mut self, presets: &[f64], viewport_width: i32) {
        self.cycle_width_impl(presets, viewport_width, PresetCycle::Up);
    }

    pub fn cycle_width_down(&mut self, presets: &[f64], viewport_width: i32) {
        self.cycle_width_impl(presets, viewport_width, PresetCycle::Down);
    }

    fn cycle_width_impl(&mut self, presets: &[f64], viewport_width: i32, cycle: PresetCycle) {
        self.maximized_column = None;
        if presets.is_empty() {
            return;
        }
        let base = self.width_base(viewport_width);
        let gap = self.gap.max(0);
        let Some(column) = self.columns.get(self.focused_column) else {
            return;
        };
        if base <= 0 {
            return;
        }
        let current_frac =
            self.effective_column_width(column).saturating_add(gap) as f64 / base as f64;

        if let Some(frac) = cycle_preset(presets, current_frac, cycle) {
            let new_width = (base as f64 * frac - gap as f64).floor() as i32;
            self.columns[self.focused_column].set_width(new_width);
        }
    }

    /// Snaps a resized column to the nearest preset, preferring the smallest preset meeting its
    /// minimum when needed.
    /// If no preset meets the minimum, uses the nearest preset.
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

    /// Snaps a resized window's height weight to the nearest preset in multi-window columns.
    /// Tabbed columns are unchanged so invisible weights cannot cause drift when returning to
    /// Vertical.
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
            if let Some(column) = self.columns.get_mut(col_idx) {
                column.set_height_weight(win_idx, weight);
            }
        }
    }

    /// Cycle the focused window's height through presets, wrapping to the smallest.
    /// No-op for single-window or tabbed columns.
    pub fn cycle_height(&mut self, presets: &[f64]) {
        self.cycle_height_impl(presets, PresetCycle::Wrap);
    }

    /// Cycles height weight upward through fractional presets.
    /// No-op for single-window or tabbed columns.
    pub fn cycle_height_up(&mut self, presets: &[f64]) {
        self.cycle_height_impl(presets, PresetCycle::Up);
    }

    /// Cycles height weight downward; no-op for single-window or tabbed columns.
    pub fn cycle_height_down(&mut self, presets: &[f64]) {
        self.cycle_height_impl(presets, PresetCycle::Down);
    }

    fn cycle_height_impl(&mut self, presets: &[f64], cycle: PresetCycle) {
        if presets.is_empty() {
            return;
        }
        let col_idx = self.focused_column;
        let win_idx = self.focused_window_in_column;
        let col = match self.columns.get_mut(col_idx) {
            Some(c) => c,
            None => return,
        };
        // Only the active tab renders, so height weights have no visible effect.
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

    /// Equalize height weights in the focused column.
    /// No-op for Tabbed columns (only one tab visible at a time).
    pub fn equalize_focused_column_heights(&mut self) {
        if let Some(col) = self.columns.get_mut(self.focused_column) {
            if col.is_tabbed() {
                return;
            }
            col.equalize_height_weights();
        }
    }

    /// Set scroll offset directly (bypasses clamping).
    pub fn set_scroll_offset(&mut self, offset: f64) {
        self.scroll_offset = offset;
    }

    pub fn set_all_column_widths(&mut self, width: i32) {
        for col in &mut self.columns {
            col.set_width(width);
        }
    }

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

    // Tabbed columns have no visible height weight to preview.
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
        if column.len() <= 1 || column.is_tabbed() {
            return None;
        }

        let outer_top = self.outer_gap_top.max(0);
        let outer_bottom = self.outer_gap_bottom.max(0);
        let gap = self.gap.max(0);
        let window_gaps = gap.saturating_mul(column.len() as i32 - 1);
        let available_height = (viewport_height - outer_top - outer_bottom - window_gaps).max(1);

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

    /// Previews the preset-snapped rectangle, temporarily changing and restoring column width and
    /// weights.
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

        let snapped_width =
            self.nearest_preset_width(col_idx, current_width, width_presets, viewport.width);
        let snapped_weight = self.nearest_preset_height_weight(
            col_idx,
            current_height,
            height_presets,
            viewport.height,
        );

        let original_width = self.columns[col_idx].width;
        let original_weights = self.columns[col_idx].height_weights.clone();

        if let Some(w) = snapped_width {
            self.columns[col_idx].set_width(w);
        }
        if let Some(weight) = snapped_weight {
            self.columns[col_idx].set_height_weight(win_idx, weight);
        }

        let placements = self.compute_placements(viewport);
        let rect = placements
            .iter()
            .find(|p| p.window_id == window_id)
            .map(|p| p.rect);

        self.columns[col_idx].width = original_width;
        self.columns[col_idx].height_weights = original_weights;

        rect
    }
}
