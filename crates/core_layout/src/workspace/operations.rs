use crate::*;

use crate::workspace::Workspace;

impl Workspace {
    pub fn column_x(&self, column_index: usize) -> i32 {
        self.column_x_for_row(self.focused_row, column_index)
    }

    pub fn column_x_for_row(&self, row_idx: usize, column_index: usize) -> i32 {
        self.column_x_with_minimized_handling_for_row(row_idx, column_index, true)
    }

    /// Compute the X position of a column in the focused row, optionally skipping minimized columns.
    pub fn column_x_with_minimized_handling(&self, column_index: usize, skip_minimized: bool) -> i32 {
        self.column_x_with_minimized_handling_for_row(self.focused_row, column_index, skip_minimized)
    }

    /// Compute the X position of a column in a specific row.
    pub fn column_x_with_minimized_handling_for_row(
        &self,
        row_idx: usize,
        column_index: usize,
        skip_minimized: bool,
    ) -> i32 {
        let gap = self.gap.max(0);
        let Some(row) = self.rows.get(row_idx) else {
            return 0;
        };

        let mut x = 0;
        for (i, col) in row.columns.iter().enumerate() {
            if i == column_index {
                return x;
            }
            if skip_minimized && !self.is_column_active(col) {
                continue;
            }
            x = x
                .saturating_add(self.effective_column_width(col))
                .saturating_add(gap);
        }
        x
    }

    /// Get the x-coordinate and width of the focused column in the focused row.
    fn focused_column_bounds(&self) -> Option<(i32, i32)> {
        let fr = self.focused_row;
        let row = self.rows.get(fr)?;
        row.columns.get(row.focused_column).map(|col| {
            let x = self.column_x_for_row(fr, row.focused_column);
            (x, self.effective_column_width(col))
        })
    }

    /// The width of the visible strip area inside the viewport (viewport minus outer padding).
    pub fn visible_width(&self, viewport_width: i32) -> i32 {
        viewport_width
            .saturating_sub(self.outer_gap_left.max(0))
            .saturating_sub(self.outer_gap_right.max(0))
            .max(0)
    }

    pub(super) fn scroll_bounds_for_row(
        &self,
        row_idx: usize,
        viewport_width: i32,
        min_scroll: f64,
        max_scroll: f64,
    ) -> (f64, f64) {
        if self.center_single_column && self.fullscreen_window.is_none() {
            if let Some(row) = self.rows.get(row_idx) {
                let mut active = row
                    .columns
                    .iter()
                    .filter(|column| self.is_column_active(column));
                if let Some(column) = active.next() {
                    let width = self.effective_column_width(column);
                    let visible_width = self.visible_width(viewport_width);
                    if active.next().is_none() && width <= visible_width {
                        let offset = (-(visible_width - width) / 2) as f64;
                        return (offset, offset);
                    }
                }
            }
        }
        (min_scroll, max_scroll)
    }


    pub(super) fn clamp_scroll_offset_for_row(
        &self,
        row_idx: usize,
        offset: f64,
        viewport_width: i32,
        min_scroll: f64,
        max_scroll: f64,
    ) -> f64 {
        let (min_scroll, max_scroll) =
            self.scroll_bounds_for_row(row_idx, viewport_width, min_scroll, max_scroll);
        offset.clamp(min_scroll, max_scroll)
    }

    pub(super) fn clamp_scroll_offset(
        &self,
        offset: f64,
        viewport_width: i32,
        min_scroll: f64,
        max_scroll: f64,
    ) -> f64 {
        self.clamp_scroll_offset_for_row(
            self.focused_row,
            offset,
            viewport_width,
            min_scroll,
            max_scroll,
        )
    }

    /// Whether the focused column should be centered under current centering mode.
    fn should_center(&self, col_width: i32, vis_w: i32) -> bool {
        match self.centering_mode {
            CenteringMode::Center => true,
            CenteringMode::JustInView => false,
            CenteringMode::OnOverflow => col_width > vis_w,
        }
    }

    /// Ensure the focused column is visible in the viewport.
    pub fn ensure_focused_visible(&mut self, viewport_width: i32) {
        let fr = self.focused_row;
        self.ensure_focused_visible_for_row(fr, viewport_width);
    }

    /// Ensure the focused column of a specific row is visible.
    pub fn ensure_focused_visible_for_row(&mut self, row_idx: usize, viewport_width: i32) {
        let Some(row) = self.rows.get(row_idx) else {
            return;
        };
        if row.columns.is_empty() {
            return;
        }

        let Some(col) = row.columns.get(row.focused_column) else {
            return;
        };
        let col_x = self.column_x_for_row(row_idx, row.focused_column);
        let col_width = self.effective_column_width(col);
        let vis_w = self.visible_width(viewport_width);

        let should_center = self.should_center(col_width, vis_w);
        let row_total_w = self.total_width_for_row(row_idx);
        let current_scroll = self.rows[row_idx].scroll_offset;

        let unconstrained_offset = if should_center {
            let col_center = col_x.saturating_add(col_width / 2);
            (col_center.saturating_sub(vis_w / 2)) as f64
        } else {
            let scroll_left = current_scroll.round() as i32;
            let scroll_right = scroll_left.saturating_add(vis_w);
            let col_right = col_x.saturating_add(col_width);

            if col_x < scroll_left {
                col_x as f64
            } else if col_right > scroll_right {
                col_right.saturating_sub(vis_w) as f64
            } else {
                current_scroll
            }
        };

        let max_scroll = (row_total_w - vis_w).max(0) as f64;
        let min_scroll = if self.center_past_edges {
            f64::NEG_INFINITY
        } else {
            0.0
        };
        self.rows[row_idx].scroll_offset = self.clamp_scroll_offset_for_row(
            row_idx,
            unconstrained_offset,
            viewport_width,
            min_scroll,
            max_scroll,
        );
    }

    /// Resize the focused column by a delta amount.
    pub fn resize_focused_column(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        let fr = self.focused_row;
        let foc = self.rows[fr].focused_column;
        self.rows[fr].maximized_column = None;
        if let Some(column) = self.rows[fr].columns.get(foc) {
            let eff_w = self.effective_column_width(column);
            let new_width = eff_w.saturating_add(delta);
            let column = &mut self.rows[fr].columns[foc];
            column.set_width(new_width);
            for wid in column.windows() {
                self.window_min_widths.remove(wid);
                self.window_min_heights.remove(wid);
            }
        }
    }

    /// Move the focused column left (swap with column to its left).
    pub fn move_column_left(&mut self) {
        let fr = self.focused_row;
        let row = &mut self.rows[fr];
        if row.focused_column > 0 {
            row.columns.swap(row.focused_column, row.focused_column - 1);
            row.focused_column -= 1;
        }
    }

    /// Move the focused column right (swap with column to its right).
    pub fn move_column_right(&mut self) {
        let fr = self.focused_row;
        let row = &mut self.rows[fr];
        if row.focused_column + 1 < row.columns.len() {
            row.columns.swap(row.focused_column, row.focused_column + 1);
            row.focused_column += 1;
        }
    }

    /// Move the focused column to the start of the strip.
    pub fn move_column_to_start(&mut self) {
        let fr = self.focused_row;
        let foc = self.rows[fr].focused_column;
        self.reorder_column(foc, 0);
    }

    /// Move the focused column to the end of the strip.
    pub fn move_column_to_end(&mut self) {
        let fr = self.focused_row;
        let len = self.rows[fr].columns.len();
        if len > 0 {
            let foc = self.rows[fr].focused_column;
            self.reorder_column(foc, len - 1);
        }
    }

    /// Move a column from one index to another in the focused row.
    pub fn reorder_column(&mut self, from: usize, to: usize) {
        let fr = self.focused_row;
        self.reorder_column_in_row(fr, from, to);
    }

    /// Move a column from one index to another in a specific row.
    pub fn reorder_column_in_row(&mut self, row_idx: usize, from: usize, to: usize) {
        let Some(row) = self.rows.get_mut(row_idx) else {
            return;
        };
        if from == to || from >= row.columns.len() || to >= row.columns.len() {
            return;
        }
        let column = row.columns.remove(from);
        row.columns.insert(to, column);

        if row.focused_column == from {
            row.focused_column = to;
        } else if from < to {
            if row.focused_column > from && row.focused_column <= to {
                row.focused_column -= 1;
            }
        } else if row.focused_column >= to && row.focused_column < from {
            row.focused_column += 1;
        }
        self.clamp_focus_indices_for_row(row_idx);
    }

    /// Remove an entire column from the focused row and return it.
    pub fn remove_column(&mut self, index: usize) -> Option<Column> {
        let fr = self.focused_row;
        self.remove_column_in_row(fr, index)
    }

    /// Remove an entire column from a specific row and return it.
    pub fn remove_column_in_row(&mut self, row_idx: usize, index: usize) -> Option<Column> {
        let row = self.rows.get_mut(row_idx)?;
        if index >= row.columns.len() {
            return None;
        }
        let col = row.columns.remove(index);
        for wid in col.windows() {
            self.minimized_windows.remove(wid);
            if self.fullscreen_window == Some(*wid) {
                self.fullscreen_window = None;
            }
        }
        if row.columns.is_empty() {
            row.focused_column = 0;
            row.focused_window_in_column = 0;
            row.scroll_offset = 0.0;
        } else {
            if row.focused_column > index {
                row.focused_column -= 1;
            }
            // Outside fullscreen, centering needs a viewport; defer bounds repair
            // to reveal/reconcile so removal preserves the animation's start.
            if !self.center_single_column || self.fullscreen_window.is_some() {
                let max_scroll = self.total_width_for_row(row_idx).max(0) as f64;
                let row = &mut self.rows[row_idx];
                row.scroll_offset = row.scroll_offset.clamp(0.0, max_scroll);
            }
        }
        self.clamp_focus_indices_for_row(row_idx);
        Some(col)
    }

    /// Insert a column at the given index in the focused row.
    pub fn insert_column_at(&mut self, column: Column, index: usize) {
        let fr = self.focused_row;
        self.insert_column_at_row(column, fr, index);
    }

    /// Insert a column at the given index in a specific row.
    pub fn insert_column_at_row(&mut self, column: Column, row_idx: usize, index: usize) {
        if column.is_empty() {
            return;
        }
        if column.windows().iter().any(|wid| self.contains_window(*wid)) {
            return;
        }
        let Some(row) = self.rows.get_mut(row_idx) else {
            return;
        };
        let clamped = index.min(row.columns.len());
        let was_empty = row.columns.is_empty();
        row.columns.insert(clamped, column);
        if !was_empty && row.focused_column >= clamped {
            row.focused_column += 1;
        }
        self.clamp_focus_indices_for_row(row_idx);
    }

    /// Move the focused window to the column on the left (joining it).
    pub fn move_window_left(&mut self) {
        let fr = self.focused_row;
        let row = &mut self.rows[fr];
        if row.columns.is_empty() {
            return;
        }
        if row.focused_column == 0 {
            self.expel_to_left();
            return;
        }
        let Some(wid) = row.columns[row.focused_column].remove_at_index(row.focused_window_in_column)
        else {
            return;
        };
        let source_empty = row.columns[row.focused_column].is_empty();
        if source_empty {
            row.columns.remove(row.focused_column);
        }
        let target_idx = row.focused_column - 1;
        row.columns[target_idx].add_window(wid);
        row.focused_column = target_idx;
        row.focused_window_in_column = row.columns[target_idx].len() - 1;
        self.sync_active_tab_to_focus();
    }

    /// Move the focused window to the column on the right (joining it).
    pub fn move_window_right(&mut self) {
        let fr = self.focused_row;
        let row = &mut self.rows[fr];
        if row.focused_column + 1 >= row.columns.len() {
            self.expel_to_right();
            return;
        }
        let Some(wid) = row.columns[row.focused_column].remove_at_index(row.focused_window_in_column)
        else {
            return;
        };
        let source_empty = row.columns[row.focused_column].is_empty();
        if source_empty {
            row.columns.remove(row.focused_column);
            row.columns[row.focused_column].add_window(wid);
            row.focused_window_in_column = row.columns[row.focused_column].len() - 1;
        } else {
            let right_idx = row.focused_column + 1;
            row.columns[right_idx].add_window(wid);
            row.focused_column = right_idx;
            row.focused_window_in_column = row.columns[right_idx].len() - 1;
        }
        self.sync_active_tab_to_focus();
    }

    /// Push the focused window out to a new column on the left.
    pub fn expel_to_left(&mut self) {
        let fr = self.focused_row;
        let row = &mut self.rows[fr];
        if row.columns.is_empty() || row.columns[row.focused_column].len() <= 1 {
            return;
        }
        let Some(wid) = row.columns[row.focused_column].remove_at_index(row.focused_window_in_column)
        else {
            return;
        };
        let old_len = row.columns[row.focused_column].len();
        if row.focused_window_in_column >= old_len {
            row.focused_window_in_column = old_len.saturating_sub(1);
        }
        let width = row.columns[row.focused_column].width();
        let mut new_col = Column::new(wid, width);
        new_col.width_fraction_cache = row.columns[row.focused_column]
            .width_fraction_cache
            .clone();
        row.columns.insert(row.focused_column, new_col);
        row.focused_window_in_column = 0;
    }

    /// Push the focused window out to a new column on the right.
    pub fn expel_to_right(&mut self) {
        let fr = self.focused_row;
        let row = &mut self.rows[fr];
        if row.columns.is_empty() || row.columns[row.focused_column].len() <= 1 {
            return;
        }
        let Some(wid) = row.columns[row.focused_column].remove_at_index(row.focused_window_in_column)
        else {
            return;
        };
        let old_len = row.columns[row.focused_column].len();
        if row.focused_window_in_column >= old_len {
            row.focused_window_in_column = old_len.saturating_sub(1);
        }
        let width = row.columns[row.focused_column].width();
        let mut new_col = Column::new(wid, width);
        new_col.width_fraction_cache = row.columns[row.focused_column]
            .width_fraction_cache
            .clone();
        row.columns.insert(row.focused_column + 1, new_col);
        row.focused_column += 1;
        row.focused_window_in_column = 0;
    }

    /// Pull the top window of the column to the right into the focused column.
    pub fn consume_from_right(&mut self) {
        let fr = self.focused_row;
        let row = &mut self.rows[fr];
        let right = row.focused_column + 1;
        if right >= row.columns.len() {
            return;
        }
        let Some(wid) = row.columns[right].remove_at_index(0) else {
            return;
        };
        if row.columns[right].is_empty() {
            row.columns.remove(right);
        }
        row.columns[row.focused_column].add_window(wid);
        row.focused_window_in_column = row.columns[row.focused_column].len().saturating_sub(1);
        self.sync_active_tab_to_focus();
    }

    /// Pull the top window of the column to the left into the focused column.
    pub fn consume_from_left(&mut self) {
        let fr = self.focused_row;
        let row = &mut self.rows[fr];
        if row.focused_column == 0 {
            return;
        }
        let left = row.focused_column - 1;
        let Some(wid) = row.columns[left].remove_at_index(0) else {
            return;
        };
        if row.columns[left].is_empty() {
            row.columns.remove(left);
            row.focused_column -= 1;
        }
        row.columns[row.focused_column].add_window(wid);
        row.focused_window_in_column = row.columns[row.focused_column].len().saturating_sub(1);
        self.sync_active_tab_to_focus();
    }

    /// Swap the focused window with the one above in the same column.
    pub fn move_window_up_in_column(&mut self) {
        let fr = self.focused_row;
        let row = &mut self.rows[fr];
        if row.focused_window_in_column == 0 {
            return;
        }
        let cur = row.focused_window_in_column;
        let col = row.focused_column;
        row.columns[col].swap_windows(cur, cur - 1);
        row.focused_window_in_column -= 1;
        self.sync_active_tab_to_focus();
    }

    /// Move the focused window up in its column, or to the row above if at the top
    /// of the column and `allow_cross_row` is true (D5).
    pub fn move_window_up(&mut self, allow_cross_row: bool) {
        let fr = self.focused_row;
        let at_top = self.rows.get(fr).is_some_and(|r| r.focused_window_in_column == 0);
        if at_top {
            if allow_cross_row && fr > 0 {
                let _ = self.move_window_to_row_up();
            }
        } else {
            self.move_window_up_in_column();
        }
    }

    /// Swap the focused window with the one below in the same column.
    pub fn move_window_down_in_column(&mut self) {
        let fr = self.focused_row;
        let row = &mut self.rows[fr];
        let cur = row.focused_window_in_column;
        let col = row.focused_column;
        if cur + 1 >= row.columns[col].len() {
            return;
        }
        row.columns[col].swap_windows(cur, cur + 1);
        row.focused_window_in_column += 1;
        self.sync_active_tab_to_focus();
    }

    /// Move the focused window down in its column, or to the row below if at the bottom
    /// of the column and `allow_cross_row` is true (D5).
    pub fn move_window_down(&mut self, allow_cross_row: bool) {
        let fr = self.focused_row;
        let at_bottom = self.rows.get(fr).is_some_and(|r| {
            r.columns.get(r.focused_column).is_some_and(|col| {
                r.focused_window_in_column + 1 >= col.len()
            })
        });
        if at_bottom {
            if allow_cross_row && fr + 1 < self.rows.len() {
                let _ = self.move_window_to_row_down();
            }
        } else {
            self.move_window_down_in_column();
        }
    }

    /// Scroll the focused row's strip by a pixel delta.
    pub fn scroll_by(&mut self, delta: f64, viewport_width: i32) {
        let fr = self.focused_row;
        self.scroll_row_by(fr, delta, viewport_width);
    }

    /// Scroll a specific row's strip by a pixel delta.
    pub fn scroll_row_by(&mut self, row_idx: usize, delta: f64, viewport_width: i32) {
        let row_total_w = self.total_width_for_row(row_idx);
        let vis_w = self.visible_width(viewport_width);
        let Some(row) = self.rows.get_mut(row_idx) else {
            return;
        };
        if let Some(anim) = row.active_animation.take() {
            row.scroll_offset = anim.current_offset();
        }
        let safe_delta = if delta.is_finite() { delta } else { 0.0 };
        let new_offset = self.rows[row_idx].scroll_offset + safe_delta;
        let max_scroll = (row_total_w - vis_w).max(0);
        self.rows[row_idx].scroll_offset = self.clamp_scroll_offset_for_row(
            row_idx,
            new_offset,
            viewport_width,
            0.0,
            max_scroll as f64,
        );
    }

    /// Repair bounds without revealing focus across all rows.
    pub fn reconcile_scroll_bounds(&mut self, viewport_width: i32) {
        let vis_w = self.visible_width(viewport_width);
        for row_idx in 0..self.rows.len() {
            let row_total_w = self.total_width_for_row(row_idx);
            let mut min_scroll = 0.0_f64;
            let mut max_scroll = (row_total_w - vis_w).max(0) as f64;
            if self.center_past_edges {
                let row = &self.rows[row_idx];
                let mut active = row
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, column)| self.is_column_active(column));
                if let Some((first_idx, first)) = active.next() {
                    let (last_idx, last) = active.next_back().unwrap_or((first_idx, first));
                    let first_center = self.effective_column_width(first) / 2;
                    let last_center = self
                        .column_x_for_row(row_idx, last_idx)
                        .saturating_add(self.effective_column_width(last) / 2);
                    min_scroll = min_scroll.min(first_center.saturating_sub(vis_w / 2) as f64);
                    max_scroll = max_scroll.max(last_center.saturating_sub(vis_w / 2) as f64);
                }
            }
            let (min_scroll, max_scroll) =
                self.scroll_bounds_for_row(row_idx, viewport_width, min_scroll, max_scroll);
            let current = self.effective_scroll_offset_for_row(row_idx);
            let target = self.rows[row_idx]
                .active_animation
                .as_ref()
                .map_or(current, |anim| anim.target());
            let bounds = min_scroll..=max_scroll;
            if bounds.contains(&target)
                && (bounds.contains(&current)
                    || (self.center_single_column
                        && self.fullscreen_window.is_none()
                        && self.rows[row_idx].active_animation.is_some()))
            {
                continue;
            }
            let row = &mut self.rows[row_idx];
            if let Some(anim) = row.active_animation.take() {
                row.scroll_offset = anim.current_offset();
            }
            row.scroll_offset = row.scroll_offset.clamp(min_scroll, max_scroll);
        }
    }

    // ========================================================================
    // Animation Methods
    // ========================================================================

    /// Check if a scroll animation is currently active on any row.
    pub fn is_animating(&self) -> bool {
        self.rows.iter().any(|r| r.is_animating())
    }

    /// Check if a scroll animation is active on a specific row.
    pub fn is_row_animating(&self, row_idx: usize) -> bool {
        self.rows.get(row_idx).is_some_and(|r| r.is_animating())
    }

    /// Alias for `is_row_animating`.
    pub fn is_animating_for_row(&self, row_idx: usize) -> bool {
        self.is_row_animating(row_idx)
    }

    /// Get current effective scroll offset for the focused row.
    pub fn effective_scroll_offset(&self) -> f64 {
        self.effective_scroll_offset_for_row(self.focused_row)
    }

    /// Get current effective scroll offset for a specific row.
    pub fn effective_scroll_offset_for_row(&self, row_idx: usize) -> f64 {
        self.rows
            .get(row_idx)
            .map(|r| {
                r.active_animation
                    .as_ref()
                    .map_or(r.scroll_offset, |anim| anim.current_offset())
            })
            .unwrap_or(0.0)
    }

    /// Start an animated scroll to a target offset on the focused row.
    pub fn start_scroll_animation(
        &mut self,
        target: f64,
        viewport_width: i32,
        duration_ms: Option<u64>,
        easing: Option<Easing>,
    ) {
        let fr = self.focused_row;
        self.start_scroll_animation_for_row(fr, target, viewport_width, duration_ms, easing);
    }

    /// Start an animated scroll to a target offset on a specific row.
    pub fn start_scroll_animation_for_row(
        &mut self,
        row_idx: usize,
        target: f64,
        viewport_width: i32,
        duration_ms: Option<u64>,
        easing: Option<Easing>,
    ) {
        let vis_w = self.visible_width(viewport_width);
        let max_scroll = (self.total_width_for_row(row_idx) - vis_w).max(0);
        let clamped_target =
            self.clamp_scroll_offset_for_row(row_idx, target, viewport_width, 0.0, max_scroll as f64);
        self.animate_scroll_to_for_row(row_idx, clamped_target, duration_ms, easing);
    }

    pub fn animate_scroll_to(
        &mut self,
        target: f64,
        duration_ms: Option<u64>,
        easing: Option<Easing>,
    ) {
        let fr = self.focused_row;
        self.animate_scroll_to_for_row(fr, target, duration_ms, easing);
    }

    pub fn animate_scroll_to_for_row(
        &mut self,
        row_idx: usize,
        target: f64,
        duration_ms: Option<u64>,
        easing: Option<Easing>,
    ) {
        let start = self.effective_scroll_offset_for_row(row_idx);
        let Some(row) = self.rows.get_mut(row_idx) else {
            return;
        };

        if (start - target).abs() < 0.5 {
            row.scroll_offset = target;
            row.active_animation = None;
            return;
        }

        let duration = duration_ms.unwrap_or(self.scroll_duration_ms);
        let ease = easing.unwrap_or(self.scroll_easing);

        row.active_animation = Some(ScrollAnimation::new(start, target, duration, ease));
    }

    /// Advance active animations across all rows by delta time in ms.
    /// Returns true if any animation is still active.
    pub fn tick_animation(&mut self, delta_ms: u64) -> bool {
        let mut still_running = false;
        for row in &mut self.rows {
            if let Some(anim) = &mut row.active_animation {
                if anim.tick(delta_ms) {
                    still_running = true;
                } else {
                    row.scroll_offset = anim.target();
                    row.active_animation = None;
                }
            }
        }
        still_running
    }

    /// Stop animations on all rows and snap to targets.
    pub fn stop_animation(&mut self) {
        for row in &mut self.rows {
            if let Some(anim) = row.active_animation.take() {
                row.scroll_offset = anim.target();
            }
        }
    }

    /// Cancel animations on all rows and stay at current positions.
    pub fn cancel_animation(&mut self) {
        for row in &mut self.rows {
            if let Some(anim) = row.active_animation.take() {
                row.scroll_offset = anim.current_offset();
            }
        }
    }

    /// Ensure the focused column is visible with animation on the focused row.
    pub fn ensure_focused_visible_animated(&mut self, viewport_width: i32) {
        if self.reduce_motion {
            self.stop_animation();
            self.ensure_focused_visible(viewport_width);
            return;
        }
        let fr = self.focused_row;
        let Some(row) = self.rows.get(fr) else {
            return;
        };
        if row.columns.is_empty() {
            return;
        }

        let Some((col_x, col_width)) = self.focused_column_bounds() else {
            return;
        };

        let vis_w = self.visible_width(viewport_width);
        let target_offset = if self.should_center(col_width, vis_w) {
            let col_center = col_x.saturating_add(col_width / 2);
            (col_center.saturating_sub(vis_w / 2)) as f64
        } else {
            let current = self.rows[fr]
                .active_animation
                .as_ref()
                .map_or(self.rows[fr].scroll_offset, |anim| anim.target());
            let scroll_left = current.round() as i32;
            let scroll_right = scroll_left.saturating_add(vis_w);
            let col_right = col_x.saturating_add(col_width);

            if col_x < scroll_left {
                col_x as f64
            } else if col_right > scroll_right {
                col_right.saturating_sub(vis_w) as f64
            } else {
                let max_scroll = (self.total_width_for_row(fr) - vis_w).max(0) as f64;
                if current < -0.5 && !self.center_past_edges {
                    0.0
                } else if current > max_scroll + 0.5 {
                    max_scroll
                } else if self.center_single_column {
                    current
                } else {
                    return;
                }
            }
        };

        let max_scroll = (self.total_width_for_row(fr) - vis_w).max(0) as f64;
        let min_scroll = if self.center_past_edges {
            f64::NEG_INFINITY
        } else {
            0.0
        };
        let target =
            self.clamp_scroll_offset_for_row(fr, target_offset, viewport_width, min_scroll, max_scroll);
        self.animate_scroll_to_for_row(fr, target, None, None);
    }

    /// Center the focused column in the viewport with animation.
    pub fn center_focused_column_animated(&mut self, viewport_width: i32) {
        let fr = self.focused_row;
        let Some(row) = self.rows.get(fr) else {
            return;
        };
        if row.columns.is_empty() {
            return;
        }

        let Some((col_x, col_width)) = self.focused_column_bounds() else {
            return;
        };

        let vis_w = self.visible_width(viewport_width);
        let col_center = col_x.saturating_add(col_width / 2);
        let target = self.clamp_scroll_offset(
            (col_center - vis_w / 2) as f64,
            viewport_width,
            f64::NEG_INFINITY,
            f64::INFINITY,
        );

        if self.center_past_edges {
            if self.reduce_motion {
                self.stop_animation();
                self.rows[fr].scroll_offset = target;
                return;
            }
            self.animate_scroll_to_for_row(fr, target, None, None);
        } else {
            if self.reduce_motion {
                self.stop_animation();
                let max_scroll = (self.total_width_for_row(fr) - vis_w).max(0);
                self.rows[fr].scroll_offset =
                    self.clamp_scroll_offset_for_row(fr, target, viewport_width, 0.0, max_scroll as f64);
                return;
            }
            self.start_scroll_animation_for_row(fr, target, viewport_width, None, None);
        }
    }
}
