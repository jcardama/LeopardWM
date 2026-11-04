use std::cmp::Reverse;

use crate::*;

use crate::workspace::Workspace;

/// Where focus moves among a workspace's floating windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatingFocus {
    /// The nearest window whose center lies to the left.
    Left,
    /// The nearest window whose center lies to the right.
    Right,
    /// The nearest window whose center lies above.
    Up,
    /// The nearest window whose center lies below.
    Down,
    /// The window whose left edge is furthest left.
    Leftmost,
    /// The window whose left edge is furthest right.
    Rightmost,
    /// The next window by left edge, then top edge, wrapping at the end.
    Next,
    /// The previous window by left edge, then top edge, wrapping at the start.
    Prev,
}

impl Workspace {
    /// Insert a new window as a new column to the right of the focused column.
    /// Column width is clamped to MIN_COLUMN_WIDTH (100px) minimum.
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::DuplicateWindow` if the window ID already exists.
    pub fn insert_window(
        &mut self,
        window_id: WindowId,
        width: Option<i32>,
    ) -> Result<(), LayoutError> {
        if self.contains_window(window_id) {
            return Err(LayoutError::DuplicateWindow(window_id));
        }

        let column_width = width
            .unwrap_or(self.default_column_width)
            .max(MIN_COLUMN_WIDTH);
        let new_column = Column::new(window_id, column_width);

        if self.columns.is_empty() {
            self.columns.push(new_column);
            self.focused_column = 0;
        } else {
            let insert_pos = self.focused_column + 1;
            self.columns.insert(insert_pos, new_column);
            self.focused_column = insert_pos;
        }
        self.focused_window_in_column = 0;

        debug_assert!(
            self.focused_column < self.columns.len(),
            "Invariant violation: focused_column out of bounds after insert"
        );

        Ok(())
    }

    /// Insert a single-window column at `index` (clamped to the column count), then focus it.
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::DuplicateWindow` if the window ID already exists.
    pub fn insert_window_at_column(
        &mut self,
        window_id: WindowId,
        width: Option<i32>,
        index: usize,
    ) -> Result<(), LayoutError> {
        if self.contains_window(window_id) {
            return Err(LayoutError::DuplicateWindow(window_id));
        }
        let column_width = width
            .unwrap_or(self.default_column_width)
            .max(MIN_COLUMN_WIDTH);
        self.insert_column_at(Column::new(window_id, column_width), index);
        let clamped = index.min(self.columns.len().saturating_sub(1));
        self.focused_column = clamped;
        self.focused_window_in_column = 0;
        Ok(())
    }

    /// Append a single-window column without changing focus unless the workspace is empty.
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::DuplicateWindow` if the window ID already exists.
    pub fn append_window_no_focus(
        &mut self,
        window_id: WindowId,
        width: Option<i32>,
    ) -> Result<(), LayoutError> {
        let saved_col = self.focused_column;
        let saved_win = self.focused_window_in_column;
        let was_empty = self.columns.is_empty();
        self.insert_window_at_column(window_id, width, self.columns.len())?;
        // Appending preserves existing focus indices; an empty workspace keeps the new focus.
        if !was_empty {
            self.focused_column = saved_col;
            self.focused_window_in_column = saved_win;
        }
        Ok(())
    }

    /// Insert a window as a new column at `index` (clamped) without moving the
    /// focused column; later columns shift right and the prior focus follows its
    /// window. On an empty workspace the new column becomes focused.
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::DuplicateWindow` if the window ID already exists.
    pub fn insert_window_at_column_no_focus(
        &mut self,
        window_id: WindowId,
        width: Option<i32>,
        index: usize,
    ) -> Result<(), LayoutError> {
        if self.contains_window(window_id) {
            return Err(LayoutError::DuplicateWindow(window_id));
        }
        let column_width = width
            .unwrap_or(self.default_column_width)
            .max(MIN_COLUMN_WIDTH);
        // insert_column_at shifts focused_column right when inserting at or
        // before it, so the prior focus is preserved without an override.
        self.insert_column_at(Column::new(window_id, column_width), index);
        Ok(())
    }

    /// Insert after the focused column, preserving focus unless the workspace is empty.
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::DuplicateWindow` if the window ID already exists.
    pub fn insert_window_no_focus(
        &mut self,
        window_id: WindowId,
        width: Option<i32>,
    ) -> Result<(), LayoutError> {
        let saved_col = self.focused_column;
        let saved_win = self.focused_window_in_column;

        self.insert_window(window_id, width)?;

        // Insertion after the focused column preserves its indices; an empty workspace keeps the
        // new focus.
        if saved_col < self.columns.len() && self.columns.len() > 1 {
            self.focused_column = saved_col;
            self.focused_window_in_column = saved_win;
        }

        Ok(())
    }

    /// Insert a window into an existing column (stacking).
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::ColumnOutOfBounds` if the column index is invalid.
    /// Returns `LayoutError::DuplicateWindow` if the window ID already exists.
    pub fn insert_window_in_column(
        &mut self,
        window_id: WindowId,
        column_index: usize,
    ) -> Result<(), LayoutError> {
        if self.contains_window(window_id) {
            return Err(LayoutError::DuplicateWindow(window_id));
        }

        if column_index >= self.columns.len() {
            return Err(LayoutError::ColumnOutOfBounds(
                column_index,
                self.columns.len().saturating_sub(1),
            ));
        }

        // Composition changes invalidate learned constraints; defer clearing so paused or timed-out
        // layouts retain them.
        for &wid in self.columns[column_index].windows() {
            self.pending_min_size_clears.insert(wid);
        }

        self.columns[column_index].add_window(window_id);
        Ok(())
    }

    /// Insert a window at a specific position within a column.
    /// `window_index` is clamped to the column length.
    pub fn insert_window_in_column_at(
        &mut self,
        window_id: WindowId,
        column_index: usize,
        window_index: usize,
    ) -> Result<(), LayoutError> {
        if self.contains_window(window_id) {
            return Err(LayoutError::DuplicateWindow(window_id));
        }

        if column_index >= self.columns.len() {
            return Err(LayoutError::ColumnOutOfBounds(
                column_index,
                self.columns.len().saturating_sub(1),
            ));
        }

        // Composition changes invalidate learned constraints; defer clearing so paused or timed-out
        // layouts retain them.
        for &wid in self.columns[column_index].windows() {
            self.pending_min_size_clears.insert(wid);
        }

        let clamped = window_index.min(self.columns[column_index].len());
        self.columns[column_index].insert_at(clamped, window_id);
        Ok(())
    }

    /// Remove a window from the workspace.
    /// If removing the last window from a column, the column is removed.
    /// If removing the last column, the workspace becomes empty.
    ///
    /// # Focus Policy
    ///
    /// When removing a window from a stacked column:
    /// - If removed window was before the focused window, focus index decrements to stay on same window
    /// - If removed window was the focused window, focus moves to next window (or previous if at end)
    /// - If removed window was after the focused window, focus index stays the same
    pub fn remove_window(&mut self, window_id: WindowId) -> Result<(), LayoutError> {
        for (col_idx, column) in self.columns.iter_mut().enumerate() {
            if let Some(removed_idx) = column.remove_window(window_id) {
                // Removed windows must not remain in the deferred-clear set.
                self.minimized_windows.remove(&window_id);
                self.window_min_widths.remove(&window_id);
                self.window_min_heights.remove(&window_id);
                self.pending_min_size_clears.remove(&window_id);
                if self.fullscreen_window == Some(window_id) {
                    self.fullscreen_window = None;
                }
                if self
                    .maximized_column
                    .as_ref()
                    .is_some_and(|m| m.sentinel_window == window_id)
                {
                    self.maximized_column = None;
                }

                // Composition changes invalidate learned constraints; defer clearing so paused or
                // timed-out layouts retain them.
                if !column.is_empty() {
                    for &sibling in column.windows() {
                        self.pending_min_size_clears.insert(sibling);
                    }
                }

                if column.is_empty() {
                    self.columns.remove(col_idx);
                    if self.columns.is_empty() {
                        self.focused_column = 0;
                        self.focused_window_in_column = 0;
                        self.scroll_offset = 0.0;
                    } else if self.focused_column >= self.columns.len() {
                        self.focused_column = self.columns.len() - 1;
                    } else if self.focused_column > col_idx {
                        self.focused_column -= 1;
                    }
                } else {
                    if col_idx == self.focused_column {
                        let col_len = self.columns[self.focused_column].len();
                        match removed_idx.cmp(&self.focused_window_in_column) {
                            std::cmp::Ordering::Less => {
                                self.focused_window_in_column -= 1;
                            }
                            std::cmp::Ordering::Equal => {
                                if self.focused_window_in_column >= col_len {
                                    self.focused_window_in_column = col_len.saturating_sub(1);
                                }
                                // The next window slides into the removed window's index.
                            }
                            std::cmp::Ordering::Greater => {
                                // Later removals preserve the focused index.
                            }
                        }
                    }
                }

                self.clamp_focus_indices();
                self.sync_active_tab_to_focus();

                debug_assert!(
                    self.columns.is_empty() || self.focused_column < self.columns.len(),
                    "Invariant violation: focused_column out of bounds after remove"
                );
                debug_assert!(
                    self.columns.is_empty()
                        || self.focused_window_in_column < self.columns[self.focused_column].len(),
                    "Invariant violation: focused_window_in_column out of bounds after remove"
                );

                return Ok(());
            }
        }
        Err(LayoutError::WindowNotFound(window_id))
    }

    /// Move focus to the column on the left, skipping columns where all
    /// windows are minimized. Within the target column, adjusts the
    /// focused window index to a non-minimized window.
    pub fn focus_left(&mut self) {
        let start = self.focused_column;
        while self.focused_column > 0 {
            self.focused_column -= 1;
            self.land_focus_in_current_column();
            if self.has_visible_window_in_column(self.focused_column) {
                self.adjust_focus_to_visible_in_column();
                break;
            }
        }
        if !self.has_visible_window_in_column(self.focused_column) {
            self.focused_column = start;
            self.land_focus_in_current_column();
        }

        self.clamp_focus_indices();
        if self.has_visible_window_in_column(self.focused_column) {
            self.adjust_focus_to_visible_in_column();
        }
        self.sync_active_tab_to_focus();

        debug_assert!(
            self.columns.is_empty()
                || (self.focused_column < self.columns.len()
                    && self.focused_window_in_column < self.columns[self.focused_column].len()),
            "Invariant violation: focus indices out of bounds after focus_left"
        );
    }

    /// Move focus to the column on the right, skipping columns where all
    /// windows are minimized. Within the target column, adjusts the
    /// focused window index to a non-minimized window.
    pub fn focus_right(&mut self) {
        let start = self.focused_column;
        while self.focused_column + 1 < self.columns.len() {
            self.focused_column += 1;
            self.land_focus_in_current_column();
            if self.has_visible_window_in_column(self.focused_column) {
                self.adjust_focus_to_visible_in_column();
                break;
            }
        }
        if !self.has_visible_window_in_column(self.focused_column) {
            self.focused_column = start;
            self.land_focus_in_current_column();
        }

        self.clamp_focus_indices();
        if self.has_visible_window_in_column(self.focused_column) {
            self.adjust_focus_to_visible_in_column();
        }
        self.sync_active_tab_to_focus();

        debug_assert!(
            self.columns.is_empty()
                || (self.focused_column < self.columns.len()
                    && self.focused_window_in_column < self.columns[self.focused_column].len()),
            "Invariant violation: focus indices out of bounds after focus_right"
        );
    }

    /// Move focus to the first (leftmost) column that has a visible window.
    pub fn focus_start(&mut self) {
        self.focus_to_strip_end(false);
    }

    /// Move focus to the last (rightmost) column that has a visible window.
    pub fn focus_end(&mut self) {
        self.focus_to_strip_end(true);
    }

    fn focus_to_strip_end(&mut self, last: bool) {
        if self.columns.is_empty() {
            return;
        }
        let start = self.focused_column;
        self.focused_column = if last { self.columns.len() - 1 } else { 0 };
        while !self.has_visible_window_in_column(self.focused_column) {
            if last && self.focused_column > 0 {
                self.focused_column -= 1;
            } else if !last && self.focused_column + 1 < self.columns.len() {
                self.focused_column += 1;
            } else {
                self.focused_column = start;
                break;
            }
        }
        self.land_focus_in_current_column();
        self.clamp_focus_indices();
        if self.has_visible_window_in_column(self.focused_column) {
            self.adjust_focus_to_visible_in_column();
        }
        self.sync_active_tab_to_focus();

        debug_assert!(
            self.columns.is_empty()
                || (self.focused_column < self.columns.len()
                    && self.focused_window_in_column < self.columns[self.focused_column].len()),
            "Invariant violation: focus indices out of bounds after focus_to_strip_end"
        );
    }

    /// Move focus to the window above in the current column, skipping
    /// minimized windows. In a Tabbed column, cycles to the previous tab
    /// (wrapping at the start) so a single keypress walks the tab list.
    pub fn focus_up(&mut self) {
        let Some(column) = self.columns.get(self.focused_column) else {
            return;
        };
        if column.is_tabbed() && column.len() >= 2 {
            let n = column.len();
            let cur = self.focused_window_in_column;
            for offset in 1..=n {
                let target = (cur + n - offset) % n;
                if !self.minimized_windows.contains(&column.windows[target]) {
                    self.focused_window_in_column = target;
                    self.sync_active_tab_to_focus();
                    return;
                }
            }
            return;
        }
        let mut target = self.focused_window_in_column;
        while target > 0 {
            target -= 1;
            if !self.minimized_windows.contains(&column.windows[target]) {
                self.focused_window_in_column = target;
                return;
            }
        }
    }

    /// Move focus to the window below in the current column, skipping
    /// minimized windows. In a Tabbed column, cycles to the next tab
    /// (wrapping at the end) so a single keypress walks the tab list.
    pub fn focus_down(&mut self) {
        let Some(column) = self.columns.get(self.focused_column) else {
            return;
        };
        if column.is_tabbed() && column.len() >= 2 {
            let n = column.len();
            let cur = self.focused_window_in_column;
            for offset in 1..=n {
                let target = (cur + offset) % n;
                if !self.minimized_windows.contains(&column.windows[target]) {
                    self.focused_window_in_column = target;
                    self.sync_active_tab_to_focus();
                    return;
                }
            }
            return;
        }
        let mut target = self.focused_window_in_column;
        while target + 1 < column.len() {
            target += 1;
            if !self.minimized_windows.contains(&column.windows[target]) {
                self.focused_window_in_column = target;
                return;
            }
        }
    }

    /// Focus the next non-minimized window, wrapping across columns.
    /// Entering a tabbed column prefers its active tab; other columns start at the first visible
    /// window.
    pub fn focus_next(&mut self) {
        if self.columns.is_empty() {
            return;
        }
        let start_col = self.focused_column;
        let start_win = self.focused_window_in_column;

        if let Some(column) = self.columns.get(start_col) {
            let mut target = start_win;
            while target + 1 < column.len() {
                target += 1;
                if !self.minimized_windows.contains(&column.windows[target]) {
                    self.focused_window_in_column = target;
                    self.sync_active_tab_to_focus();
                    return;
                }
            }
        }

        // Move to next columns, wrapping around. Tabbed targets land on
        // their active tab; Vertical targets land on the first visible.
        let n = self.columns.len();
        for offset in 1..=n {
            let col_idx = (start_col + offset) % n;
            if self.has_visible_window_in_column(col_idx) {
                self.focused_column = col_idx;
                let landing = self
                    .columns
                    .get(col_idx)
                    .and_then(|c| c.active_tab_idx())
                    .unwrap_or(0);
                self.focused_window_in_column = landing;
                self.adjust_focus_to_visible_in_column();
                self.sync_active_tab_to_focus();
                return;
            }
        }
    }

    /// Focus the previous non-minimized window, wrapping across columns.
    /// Entering a tabbed column prefers its active tab; otherwise it lands on the last visible
    /// window.
    pub fn focus_prev(&mut self) {
        if self.columns.is_empty() {
            return;
        }
        let start_col = self.focused_column;
        let start_win = self.focused_window_in_column;

        if let Some(column) = self.columns.get(start_col) {
            let mut target = start_win;
            while target > 0 {
                target -= 1;
                if !self.minimized_windows.contains(&column.windows[target]) {
                    self.focused_window_in_column = target;
                    self.sync_active_tab_to_focus();
                    return;
                }
            }
        }

        // Move to previous columns, wrapping around. Tabbed targets land
        // on their active tab; Vertical targets land on the last visible.
        let n = self.columns.len();
        for offset in 1..=n {
            let col_idx = (start_col + n - offset) % n;
            if let Some(column) = self.columns.get(col_idx) {
                if let Some(active_idx) = column.active_tab_idx() {
                    if active_idx < column.len()
                        && !self.minimized_windows.contains(&column.windows[active_idx])
                    {
                        self.focused_column = col_idx;
                        self.focused_window_in_column = active_idx;
                        return;
                    }
                }
                for i in (0..column.len()).rev() {
                    if !self.minimized_windows.contains(&column.windows[i]) {
                        self.focused_column = col_idx;
                        self.focused_window_in_column = i;
                        self.sync_active_tab_to_focus();
                        return;
                    }
                }
            }
        }
    }

    /// Enter tabbed columns on their remembered active tab rather than carrying over the source
    /// index.
    fn land_focus_in_current_column(&mut self) {
        let Some(col) = self.columns.get(self.focused_column) else {
            return;
        };
        match col.mode {
            ColumnMode::Vertical => {
                let col_len = col.len();
                if self.focused_window_in_column >= col_len {
                    self.focused_window_in_column = col_len.saturating_sub(1);
                }
            }
            ColumnMode::Tabbed { active_idx } => {
                self.focused_window_in_column = active_idx;
            }
        }
    }

    /// Maintain the focused tabbed-column invariant: `active_idx == focused_window_in_column`.
    pub(crate) fn sync_active_tab_to_focus(&mut self) {
        let idx = self.focused_window_in_column;
        if let Some(col) = self.columns.get_mut(self.focused_column) {
            col.set_active_tab(idx);
        }
    }

    /// Toggle the focused column between Vertical and Tabbed modes without changing the focused
    /// window.
    /// Entering Tabbed activates the focused window and requires at least two windows.
    /// No-op when there is no focused column.
    pub fn toggle_focused_column_tabbed_mode(&mut self) {
        let cur_focus = self.focused_window_in_column;
        let Some(col) = self.columns.get_mut(self.focused_column) else {
            return;
        };
        match col.mode {
            ColumnMode::Vertical => {
                if col.len() < 2 {
                    return;
                }
                col.set_tabbed(cur_focus);
            }
            ColumnMode::Tabbed { .. } => {
                col.set_vertical();
            }
        }
    }

    /// Set the active tab for a Tabbed column. Returns an error if the
    /// column is out of bounds, the column is not Tabbed, or `tab_idx`
    /// is out of range.
    ///
    /// When `column == focused_column`, also moves `focused_window_in_column`
    /// to the new tab so the focused/active invariant holds. The caller is
    /// responsible for `apply_layout()` and `sync_foreground_window()` to
    /// flow the focus change out to Win32.
    pub fn set_active_tab(&mut self, column: usize, tab_idx: usize) -> Result<(), LayoutError> {
        if column >= self.columns.len() {
            return Err(LayoutError::ColumnOutOfBounds(
                column,
                self.columns.len().saturating_sub(1),
            ));
        }
        let col = &mut self.columns[column];
        if !col.is_tabbed() {
            // Caller must toggle first; we don't auto-promote.
            return Err(LayoutError::WindowIndexOutOfBounds(
                tab_idx,
                column,
                col.len().saturating_sub(1),
            ));
        }
        if tab_idx >= col.len() {
            return Err(LayoutError::WindowIndexOutOfBounds(
                tab_idx,
                column,
                col.len().saturating_sub(1),
            ));
        }
        col.set_active_tab(tab_idx);
        if column == self.focused_column {
            self.focused_window_in_column = tab_idx;
        }
        Ok(())
    }

    pub(crate) fn clamp_focus_indices(&mut self) {
        if self.columns.is_empty() {
            self.focused_column = 0;
            self.focused_window_in_column = 0;
            return;
        }

        if self.focused_column >= self.columns.len() {
            self.focused_column = self.columns.len() - 1;
        }

        let col_len = self.columns[self.focused_column].len();
        if col_len == 0 {
            self.focused_window_in_column = 0;
            return;
        }

        if self.focused_window_in_column >= col_len {
            self.focused_window_in_column = col_len - 1;
        }
    }

    fn has_visible_window_in_column(&self, col_idx: usize) -> bool {
        self.columns.get(col_idx).is_some_and(|col| {
            col.windows
                .iter()
                .any(|w| !self.minimized_windows.contains(w))
        })
    }

    /// Replace minimized focus by searching downward first, then upward within the column.
    fn adjust_focus_to_visible_in_column(&mut self) {
        let col = match self.columns.get(self.focused_column) {
            Some(c) => c,
            None => return,
        };
        let cur = self.focused_window_in_column;
        if cur < col.len() && !self.minimized_windows.contains(&col.windows[cur]) {
            return;
        }
        for i in cur..col.len() {
            if !self.minimized_windows.contains(&col.windows[i]) {
                self.focused_window_in_column = i;
                return;
            }
        }
        for i in (0..cur).rev() {
            if !self.minimized_windows.contains(&col.windows[i]) {
                self.focused_window_in_column = i;
                return;
            }
        }
    }

    pub fn focused_window(&self) -> Option<WindowId> {
        self.columns
            .get(self.focused_column)
            .and_then(|col| col.windows.get(self.focused_window_in_column))
            .copied()
    }

    /// Return a non-minimized window from the focused column, checking the focused index, then
    /// downward, then upward.
    /// Returns `None` if that column is absent or has no non-minimized windows.
    pub fn focused_visible_window(&self) -> Option<WindowId> {
        let col = self.columns.get(self.focused_column)?;
        let cur = self.focused_window_in_column;

        if let Some(&wid) = col.windows.get(cur) {
            if !self.minimized_windows.contains(&wid) {
                return Some(wid);
            }
        }

        for i in cur..col.len() {
            if !self.minimized_windows.contains(&col.windows[i]) {
                return Some(col.windows[i]);
            }
        }
        for i in (0..cur).rev() {
            if !self.minimized_windows.contains(&col.windows[i]) {
                return Some(col.windows[i]);
            }
        }
        None
    }

    pub fn focused_column_index(&self) -> usize {
        self.focused_column
    }

    pub fn focused_window_index_in_column(&self) -> usize {
        self.focused_window_in_column
    }

    /// Whether the focused window is at the top of its column, so `focus_up` /
    /// `move_window_up_in_column` can't move it within the column. A Tabbed
    /// column with 2+ windows cycles its tabs, so it is never at an edge.
    /// Used by the workspace edge-wrap behavior.
    pub fn at_column_top(&self) -> bool {
        match self.columns.get(self.focused_column) {
            Some(col) if col.is_empty() => false,
            Some(col) if col.is_tabbed() && col.len() >= 2 => false,
            Some(_) => self.focused_window_in_column == 0,
            None => false,
        }
    }

    /// Whether the focused window is at the bottom of its column, so
    /// `focus_down` / `move_window_down_in_column` can't move it within the
    /// column. A Tabbed column with 2+ windows cycles its tabs, so it is never
    /// at an edge. Used by the workspace edge-wrap behavior.
    pub fn at_column_bottom(&self) -> bool {
        match self.columns.get(self.focused_column) {
            Some(col) if col.is_empty() => false,
            Some(col) if col.is_tabbed() && col.len() >= 2 => false,
            Some(col) => self.focused_window_in_column + 1 >= col.len(),
            None => false,
        }
    }

    /// Set focus to a specific column and window index with validation.
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::ColumnOutOfBounds` if the column index is invalid.
    /// Returns `LayoutError::WindowIndexOutOfBounds` if the window index is invalid.
    pub fn set_focus(&mut self, column: usize, window_in_column: usize) -> Result<(), LayoutError> {
        if column >= self.columns.len() {
            return Err(LayoutError::ColumnOutOfBounds(
                column,
                self.columns.len().saturating_sub(1),
            ));
        }

        let col_len = self.columns[column].len();
        if window_in_column >= col_len {
            return Err(LayoutError::WindowIndexOutOfBounds(
                window_in_column,
                column,
                col_len.saturating_sub(1),
            ));
        }

        self.focused_column = column;
        self.focused_window_in_column = window_in_column;
        self.sync_active_tab_to_focus();
        Ok(())
    }

    /// Focus a window by its ID.
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::WindowNotFound` if the window is not in the workspace.
    pub fn focus_window(&mut self, window_id: WindowId) -> Result<(), LayoutError> {
        for (col_idx, column) in self.columns.iter().enumerate() {
            if let Some(win_idx) = column.windows.iter().position(|&w| w == window_id) {
                self.focused_column = col_idx;
                self.focused_window_in_column = win_idx;
                self.sync_active_tab_to_focus();
                return Ok(());
            }
        }
        Err(LayoutError::WindowNotFound(window_id))
    }

    /// The floating window that `from` should hand focus to, or None to stay put.
    ///
    /// Candidates are this workspace's floating windows that are not minimized; `from` must be
    /// one of them. Directions compare window centers on that axis only and pick the nearest
    /// window strictly in that direction. Ties go to the window earlier in the floating list.
    pub fn floating_focus_target(&self, from: WindowId, to: FloatingFocus) -> Option<WindowId> {
        let candidates: Vec<&FloatingWindow> = self
            .floating_windows
            .iter()
            .filter(|f| !self.minimized_windows.contains(&f.id))
            .collect();
        let current = candidates.iter().find(|f| f.id == from)?.rect;
        // Doubled centers keep odd sizes exact.
        let center_x = |r: &Rect| 2 * i64::from(r.x) + i64::from(r.width);
        let center_y = |r: &Rect| 2 * i64::from(r.y) + i64::from(r.height);
        let distance = |r: &Rect| match to {
            FloatingFocus::Left => center_x(&current) - center_x(r),
            FloatingFocus::Right => center_x(r) - center_x(&current),
            FloatingFocus::Up => center_y(&current) - center_y(r),
            FloatingFocus::Down => center_y(r) - center_y(&current),
            _ => 0,
        };

        let target = match to {
            FloatingFocus::Left
            | FloatingFocus::Right
            | FloatingFocus::Up
            | FloatingFocus::Down => candidates
                .iter()
                .filter_map(|f| {
                    let d = distance(&f.rect);
                    (d > 0).then_some((d, f.id))
                })
                .min_by_key(|&(d, _)| d)
                .map(|(_, id)| id),
            FloatingFocus::Leftmost => candidates.iter().min_by_key(|f| f.rect.x).map(|f| f.id),
            FloatingFocus::Rightmost => candidates
                .iter()
                .min_by_key(|f| Reverse(f.rect.x))
                .map(|f| f.id),
            FloatingFocus::Next | FloatingFocus::Prev => {
                let mut order = candidates;
                // Stable sort keeps list order among windows sharing a corner.
                order.sort_by_key(|f| (f.rect.x, f.rect.y));
                let len = order.len();
                let pos = order.iter().position(|f| f.id == from)?;
                let step = if to == FloatingFocus::Next {
                    1
                } else {
                    len - 1
                };
                Some(order[(pos + step) % len].id)
            }
        };
        target.filter(|&id| id != from)
    }
}
