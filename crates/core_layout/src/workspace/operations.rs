use crate::*;

use crate::workspace::Workspace;

impl Workspace {
    fn column_x(&self, column_index: usize) -> i32 {
        self.column_x_with_minimized_handling(column_index, true)
    }

    /// Compute the X position of a column, optionally skipping minimized columns.
    fn column_x_with_minimized_handling(
        &self,
        column_index: usize,
        skip_minimized: bool,
    ) -> i32 {
        // Defensively clamp gaps to >= 0
        let gap = self.gap.max(0);

        // Strip coordinates start at 0 — outer gaps are viewport padding,
        // not part of the scrollable strip.
        let mut x = 0;
        for (i, col) in self.columns.iter().enumerate() {
            if i == column_index {
                return x;
            }
            // Skip fully-minimized columns when requested
            if skip_minimized && !self.is_column_active(col) {
                continue;
            }
            x = x.saturating_add(col.width).saturating_add(gap);
        }
        x
    }

    /// Get the x-coordinate and width of the focused column.
    fn focused_column_bounds(&self) -> Option<(i32, i32)> {
        self.columns.get(self.focused_column).map(|col| {
            let x = self.column_x(self.focused_column);
            (x, col.width)
        })
    }

    /// The width of the visible strip area inside the viewport (viewport minus outer padding).
    pub(crate) fn visible_width(&self, viewport_width: i32) -> i32 {
        viewport_width
            .saturating_sub(self.outer_gap_left.max(0))
            .saturating_sub(self.outer_gap_right.max(0))
            .max(0)
    }

    /// Ensure the focused column is visible in the viewport.
    /// Adjusts scroll_offset according to the centering mode.
    ///
    /// Note: Negative gaps are treated as zero for calculation purposes.
    pub fn ensure_focused_visible(&mut self, viewport_width: i32) {
        if self.columns.is_empty() {
            return;
        }

        let Some((col_x, col_width)) = self.focused_column_bounds() else {
            return;
        };

        // Outer gaps are viewport padding — visible strip area is smaller.
        let vis_w = self.visible_width(viewport_width);

        match self.centering_mode {
            CenteringMode::Center => {
                let col_center = col_x.saturating_add(col_width / 2);
                self.scroll_offset = (col_center.saturating_sub(vis_w / 2)) as f64;
            }
            CenteringMode::JustInView => {
                let scroll_left = self.scroll_offset.round() as i32;
                let scroll_right = scroll_left.saturating_add(vis_w);
                let col_right = col_x.saturating_add(col_width);

                if col_x < scroll_left {
                    self.scroll_offset = col_x as f64;
                } else if col_right > scroll_right {
                    self.scroll_offset = col_right.saturating_sub(vis_w) as f64;
                }
            }
        }

        let max_scroll = (self.total_width() - vis_w).max(0);
        self.scroll_offset = self.scroll_offset.clamp(0.0, max_scroll as f64);
    }

    /// Resize the focused column by a delta amount.
    pub fn resize_focused_column(&mut self, delta: i32) {
        if let Some(column) = self.columns.get_mut(self.focused_column) {
            let new_width = column.width.saturating_add(delta).max(MIN_COLUMN_WIDTH);
            column.width = new_width;
            // Clear cached min-widths so the constraint will be re-detected
            // from the actual window size on the next apply cycle.
            for wid in column.windows() {
                self.window_min_widths.remove(wid);
            }
        }
    }

    /// Move the focused column left (swap with the column to its left).
    pub fn move_column_left(&mut self) {
        if self.focused_column > 0 {
            self.columns
                .swap(self.focused_column, self.focused_column - 1);
            self.focused_column -= 1;
        }
    }

    /// Move the focused column right (swap with the column to its right).
    pub fn move_column_right(&mut self) {
        if self.focused_column + 1 < self.columns.len() {
            self.columns
                .swap(self.focused_column, self.focused_column + 1);
            self.focused_column += 1;
        }
    }

    /// Move a column from one index to another, shifting intermediate columns.
    /// No-op if indices are equal or out of bounds.
    pub fn reorder_column(&mut self, from: usize, to: usize) {
        if from == to || from >= self.columns.len() || to >= self.columns.len() {
            return;
        }
        let column = self.columns.remove(from);
        self.columns.insert(to, column);

        // Update focused_column to track correctly after the shift.
        if self.focused_column == from {
            self.focused_column = to;
        } else if from < to {
            // Column moved forward: indices in (from, to] shifted left by 1
            if self.focused_column > from && self.focused_column <= to {
                self.focused_column -= 1;
            }
        } else {
            // Column moved backward: indices in [to, from) shifted right by 1
            if self.focused_column >= to && self.focused_column < from {
                self.focused_column += 1;
            }
        }
        self.clamp_focus_indices();
    }

    /// Remove an entire column and return it. Used for cross-monitor drag.
    /// Returns `None` if index is out of bounds.
    pub fn remove_column(&mut self, index: usize) -> Option<Column> {
        if index >= self.columns.len() {
            return None;
        }
        let col = self.columns.remove(index);
        for wid in col.windows() {
            self.minimized_windows.remove(wid);
            // Clear fullscreen if the removed column contained the fullscreen window
            if self.fullscreen_window == Some(*wid) {
                self.fullscreen_window = None;
            }
        }
        if self.columns.is_empty() {
            self.focused_column = 0;
            self.focused_window_in_column = 0;
            self.scroll_offset = 0.0;
        } else {
            if self.focused_column > index {
                self.focused_column -= 1;
            }
            // Reclamp scroll offset — the strip may have shrunk
            let max_scroll = self.total_width().max(0) as f64;
            self.scroll_offset = self.scroll_offset.clamp(0.0, max_scroll);
        }
        self.clamp_focus_indices();
        Some(col)
    }

    /// Insert a column at the given index. Used for cross-monitor drag.
    /// Index is clamped to `columns.len()`. Empty columns are rejected to
    /// preserve the invariant that all columns contain at least one window.
    pub fn insert_column_at(&mut self, column: Column, index: usize) {
        if column.is_empty() {
            return;
        }
        // Reject if any window already exists in this workspace
        if column.windows().iter().any(|wid| self.contains_window(*wid)) {
            return;
        }
        let clamped = index.min(self.columns.len());
        let was_empty = self.columns.is_empty();
        self.columns.insert(clamped, column);
        if !was_empty && self.focused_column >= clamped {
            self.focused_column += 1;
        }
        self.clamp_focus_indices();
    }

    /// Move the focused window to the column on the left (joining it).
    /// Focus follows the moved window. If the source column becomes empty it is removed.
    pub fn move_window_left(&mut self) {
        if self.focused_column == 0 {
            return;
        }
        let wid = self.columns[self.focused_column]
            .windows
            .remove(self.focused_window_in_column);
        if self.focused_window_in_column < self.columns[self.focused_column].height_weights.len() {
            self.columns[self.focused_column].height_weights.remove(self.focused_window_in_column);
        }
        self.columns[self.focused_column].equalize_height_weights();
        let source_empty = self.columns[self.focused_column].is_empty();
        if source_empty {
            self.columns.remove(self.focused_column);
        }
        // Target is now one index to the left (or same index if source was removed)
        let target_idx = self.focused_column - 1;
        self.columns[target_idx].add_window(wid);
        self.focused_column = target_idx;
        self.focused_window_in_column = self.columns[target_idx].len() - 1;
    }

    /// Move the focused window to the column on the right (joining it).
    /// Focus follows the moved window. If the source column becomes empty it is removed.
    pub fn move_window_right(&mut self) {
        if self.focused_column + 1 >= self.columns.len() {
            return;
        }
        let wid = self.columns[self.focused_column]
            .windows
            .remove(self.focused_window_in_column);
        if self.focused_window_in_column < self.columns[self.focused_column].height_weights.len() {
            self.columns[self.focused_column].height_weights.remove(self.focused_window_in_column);
        }
        self.columns[self.focused_column].equalize_height_weights();
        let source_empty = self.columns[self.focused_column].is_empty();
        if source_empty {
            self.columns.remove(self.focused_column);
            // Right column shifted left into focused_column's slot
            self.columns[self.focused_column].add_window(wid);
            self.focused_window_in_column = self.columns[self.focused_column].len() - 1;
        } else {
            // Clamp focus in source column (we don't stay there, but keep it consistent)
            let right_idx = self.focused_column + 1;
            self.columns[right_idx].add_window(wid);
            self.focused_column = right_idx;
            self.focused_window_in_column = self.columns[right_idx].len() - 1;
        }
    }

    /// Push the focused window out to a new column on the left.
    pub fn expel_to_left(&mut self) {
        if self.columns[self.focused_column].len() <= 1 {
            return;
        }
        let wid = self.columns[self.focused_column]
            .windows
            .remove(self.focused_window_in_column);
        if self.focused_window_in_column < self.columns[self.focused_column].height_weights.len() {
            self.columns[self.focused_column].height_weights.remove(self.focused_window_in_column);
        }
        self.columns[self.focused_column].equalize_height_weights();
        // Clamp focus in old column
        let old_len = self.columns[self.focused_column].len();
        if self.focused_window_in_column >= old_len {
            self.focused_window_in_column = old_len.saturating_sub(1);
        }
        let width = self.columns[self.focused_column].width();
        let new_col = Column::new(wid, width);
        self.columns.insert(self.focused_column, new_col);
        // Focus the new column (it took the current index)
        self.focused_window_in_column = 0;
    }

    /// Push the focused window out to a new column on the right.
    pub fn expel_to_right(&mut self) {
        if self.columns[self.focused_column].len() <= 1 {
            return;
        }
        let wid = self.columns[self.focused_column]
            .windows
            .remove(self.focused_window_in_column);
        if self.focused_window_in_column < self.columns[self.focused_column].height_weights.len() {
            self.columns[self.focused_column].height_weights.remove(self.focused_window_in_column);
        }
        self.columns[self.focused_column].equalize_height_weights();
        // Clamp focus in old column
        let old_len = self.columns[self.focused_column].len();
        if self.focused_window_in_column >= old_len {
            self.focused_window_in_column = old_len.saturating_sub(1);
        }
        let width = self.columns[self.focused_column].width();
        let new_col = Column::new(wid, width);
        self.columns.insert(self.focused_column + 1, new_col);
        self.focused_column += 1;
        self.focused_window_in_column = 0;
    }

    /// Swap the focused window with the one above in the same column.
    pub fn move_window_up_in_column(&mut self) {
        if self.focused_window_in_column == 0 {
            return;
        }
        self.columns[self.focused_column]
            .swap_windows(self.focused_window_in_column, self.focused_window_in_column - 1);
        self.focused_window_in_column -= 1;
    }

    /// Swap the focused window with the one below in the same column.
    pub fn move_window_down_in_column(&mut self) {
        if self.focused_window_in_column + 1 >= self.columns[self.focused_column].len() {
            return;
        }
        self.columns[self.focused_column]
            .swap_windows(self.focused_window_in_column, self.focused_window_in_column + 1);
        self.focused_window_in_column += 1;
    }

    /// Scroll the viewport by a pixel delta.
    ///
    /// Cancels any active scroll animation so the manual scroll takes effect
    /// immediately. Special float values (NaN, Infinity) are treated as zero.
    pub fn scroll_by(&mut self, delta: f64, viewport_width: i32) {
        // Cancel any in-flight animation so manual scroll is not overridden
        self.cancel_animation();
        // Treat NaN and Infinity as zero for safety
        let safe_delta = if delta.is_finite() { delta } else { 0.0 };
        self.scroll_offset += safe_delta;
        let vis_w = self.visible_width(viewport_width);
        let max_scroll = (self.total_width() - vis_w).max(0);
        self.scroll_offset = self.scroll_offset.clamp(0.0, max_scroll as f64);
    }

    // ========================================================================
    // Animation Methods
    // ========================================================================

    /// Check if a scroll animation is currently active.
    pub fn is_animating(&self) -> bool {
        self.active_animation.is_some()
    }

    /// Get the current effective scroll offset.
    /// Returns the animated offset if an animation is active, otherwise the base offset.
    pub fn effective_scroll_offset(&self) -> f64 {
        match &self.active_animation {
            Some(anim) => anim.current_offset(),
            None => self.scroll_offset,
        }
    }

    /// Start an animated scroll to a target offset.
    /// If an animation is already active, it will be cancelled and a new one started.
    pub fn start_scroll_animation(
        &mut self,
        target: f64,
        viewport_width: i32,
        duration_ms: Option<u64>,
        easing: Option<Easing>,
    ) {
        // Clamp target to valid range (visible area = viewport minus outer padding)
        let vis_w = self.visible_width(viewport_width);
        let max_scroll = (self.total_width() - vis_w).max(0);
        let clamped_target = target.clamp(0.0, max_scroll as f64);

        // Use current effective position as start (handles interrupting animations)
        let start = self.effective_scroll_offset();

        // If already at target, no animation needed
        if (start - clamped_target).abs() < 0.5 {
            self.scroll_offset = clamped_target;
            self.active_animation = None;
            return;
        }

        let duration = duration_ms.unwrap_or(DEFAULT_ANIMATION_DURATION_MS);
        let ease = easing.unwrap_or_default();

        self.active_animation = Some(ScrollAnimation::new(start, clamped_target, duration, ease));
    }

    /// Advance the active animation by the given delta time in milliseconds.
    /// Returns true if an animation is still active, false if complete or no animation.
    pub fn tick_animation(&mut self, delta_ms: u64) -> bool {
        let Some(anim) = &mut self.active_animation else {
            return false;
        };

        let still_running = anim.tick(delta_ms);

        if !still_running {
            // Animation complete - finalize scroll offset and clear animation
            self.scroll_offset = anim.target().max(0.0);
            self.active_animation = None;
            false
        } else {
            true
        }
    }

    /// Stop the current animation and snap to the target position.
    pub fn stop_animation(&mut self) {
        if let Some(anim) = self.active_animation.take() {
            self.scroll_offset = anim.target().max(0.0);
        }
    }

    /// Cancel the current animation and stay at the current position.
    pub fn cancel_animation(&mut self) {
        if let Some(anim) = self.active_animation.take() {
            self.scroll_offset = anim.current_offset().max(0.0);
        }
    }

    /// Ensure the focused column is visible with animation.
    /// Like `ensure_focused_visible` but animates the scroll instead of jumping.
    /// Snaps instantly when `reduce_motion` is set.
    pub fn ensure_focused_visible_animated(&mut self, viewport_width: i32) {
        if self.reduce_motion {
            self.stop_animation();
            self.ensure_focused_visible(viewport_width);
            return;
        }
        if self.columns.is_empty() {
            return;
        }

        let Some((col_x, col_width)) = self.focused_column_bounds() else {
            return;
        };

        let vis_w = self.visible_width(viewport_width);

        let target_offset = match self.centering_mode {
            CenteringMode::Center => {
                let col_center = col_x.saturating_add(col_width / 2);
                (col_center.saturating_sub(vis_w / 2)) as f64
            }
            CenteringMode::JustInView => {
                let current = self.effective_scroll_offset();
                let scroll_left = current.round() as i32;
                let scroll_right = scroll_left.saturating_add(vis_w);
                let col_right = col_x.saturating_add(col_width);

                if col_x < scroll_left {
                    col_x as f64
                } else if col_right > scroll_right {
                    col_right.saturating_sub(vis_w) as f64
                } else {
                    let max_scroll = (self.total_width() - vis_w).max(0) as f64;
                    if current > max_scroll + 0.5 {
                        max_scroll
                    } else {
                        return;
                    }
                }
            }
        };

        self.start_scroll_animation(target_offset, viewport_width, None, None);
    }
}
