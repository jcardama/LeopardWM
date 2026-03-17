use crate::*;

use crate::workspace::Workspace;

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
            // Insert to the right of the focused column
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

    /// Insert a window without changing the current focus.
    ///
    /// Same as `insert_window`, but preserves `focused_column` and
    /// `focused_window_in_column` so that the user's keyboard position
    /// is not stolen by the new window. Used when `focus_new_windows=false`.
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

        // insert_window inserts at saved_col + 1 (or 0 if was empty).
        // If the workspace was empty, there's nothing to restore.
        if saved_col < self.columns.len() && self.columns.len() > 1 {
            // The new column was inserted at saved_col + 1, which shifted
            // nothing before it, so saved_col is still valid.
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
                // Also clear from minimized and min-width sets
                self.minimized_windows.remove(&window_id);
                self.window_min_widths.remove(&window_id);
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

                // If column is now empty, remove it
                if column.is_empty() {
                    self.columns.remove(col_idx);
                    if self.columns.is_empty() {
                        // Workspace is now empty - reset all state
                        self.focused_column = 0;
                        self.focused_window_in_column = 0;
                        self.scroll_offset = 0.0;
                    } else if self.focused_column >= self.columns.len() {
                        self.focused_column = self.columns.len() - 1;
                    } else if self.focused_column > col_idx {
                        self.focused_column -= 1;
                    }
                } else {
                    // Adjust focused window in column if this is the focused column
                    if col_idx == self.focused_column {
                        let col_len = self.columns[self.focused_column].len();
                        match removed_idx.cmp(&self.focused_window_in_column) {
                            std::cmp::Ordering::Less => {
                                // Removed window was before focused - decrement to stay on same window
                                self.focused_window_in_column -= 1;
                            }
                            std::cmp::Ordering::Equal => {
                                // Removed the focused window - move to next (or previous if at end)
                                if self.focused_window_in_column >= col_len {
                                    self.focused_window_in_column = col_len.saturating_sub(1);
                                }
                                // If focus index is still valid, it now points to the "next" window
                                // (which slid into this position), which is the expected behavior
                            }
                            std::cmp::Ordering::Greater => {
                                // No adjustment needed
                            }
                        }
                    }
                }

                self.clamp_focus_indices();

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
            // Clamp focused window in column
            let col_len = self.columns[self.focused_column].len();
            if self.focused_window_in_column >= col_len {
                self.focused_window_in_column = col_len.saturating_sub(1);
            }
            // Skip columns where every window is minimized
            if self.has_visible_window_in_column(self.focused_column) {
                self.adjust_focus_to_visible_in_column();
                break;
            }
        }
        // If we didn't find a visible column, stay at original
        if !self.has_visible_window_in_column(self.focused_column) {
            self.focused_column = start;
            if let Some(col) = self.columns.get(self.focused_column) {
                if self.focused_window_in_column >= col.len() {
                    self.focused_window_in_column = col.len().saturating_sub(1);
                }
            }
        }

        self.clamp_focus_indices();
        if self.has_visible_window_in_column(self.focused_column) {
            self.adjust_focus_to_visible_in_column();
        }

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
            // Clamp focused window in column
            let col_len = self.columns[self.focused_column].len();
            if self.focused_window_in_column >= col_len {
                self.focused_window_in_column = col_len.saturating_sub(1);
            }
            // Skip columns where every window is minimized
            if self.has_visible_window_in_column(self.focused_column) {
                self.adjust_focus_to_visible_in_column();
                break;
            }
        }
        // If we didn't find a visible column, stay at original
        if !self.has_visible_window_in_column(self.focused_column) {
            self.focused_column = start;
            if let Some(col) = self.columns.get(self.focused_column) {
                if self.focused_window_in_column >= col.len() {
                    self.focused_window_in_column = col.len().saturating_sub(1);
                }
            }
        }

        self.clamp_focus_indices();
        if self.has_visible_window_in_column(self.focused_column) {
            self.adjust_focus_to_visible_in_column();
        }

        debug_assert!(
            self.columns.is_empty()
                || (self.focused_column < self.columns.len()
                    && self.focused_window_in_column < self.columns[self.focused_column].len()),
            "Invariant violation: focus indices out of bounds after focus_right"
        );
    }

    /// Move focus to the window above in the current column, skipping
    /// minimized windows.
    pub fn focus_up(&mut self) {
        if let Some(column) = self.columns.get(self.focused_column) {
            let mut target = self.focused_window_in_column;
            while target > 0 {
                target -= 1;
                if !self.minimized_windows.contains(&column.windows[target]) {
                    self.focused_window_in_column = target;
                    return;
                }
            }
        }
    }

    /// Move focus to the window below in the current column, skipping
    /// minimized windows.
    pub fn focus_down(&mut self) {
        if let Some(column) = self.columns.get(self.focused_column) {
            let mut target = self.focused_window_in_column;
            while target + 1 < column.len() {
                target += 1;
                if !self.minimized_windows.contains(&column.windows[target]) {
                    self.focused_window_in_column = target;
                    return;
                }
            }
        }
    }

    /// Move focus to the next window in linear order across all columns.
    ///
    /// Traverses columns left-to-right, windows top-to-bottom. When at the
    /// last window of a column, wraps to the first window of the next column.
    /// When at the last window of the last column, wraps to the first window
    /// of the first column.
    pub fn focus_next(&mut self) {
        if self.columns.is_empty() {
            return;
        }
        let start_col = self.focused_column;
        let start_win = self.focused_window_in_column;

        // Try moving down in the current column first
        if let Some(column) = self.columns.get(start_col) {
            let mut target = start_win;
            while target + 1 < column.len() {
                target += 1;
                if !self.minimized_windows.contains(&column.windows[target]) {
                    self.focused_window_in_column = target;
                    return;
                }
            }
        }

        // Move to next columns, wrapping around
        let n = self.columns.len();
        for offset in 1..=n {
            let col_idx = (start_col + offset) % n;
            if self.has_visible_window_in_column(col_idx) {
                self.focused_column = col_idx;
                self.focused_window_in_column = 0;
                self.adjust_focus_to_visible_in_column();
                return;
            }
        }
    }

    /// Move focus to the previous window in linear order across all columns.
    ///
    /// Traverses columns right-to-left, windows bottom-to-top. When at the
    /// first window of a column, wraps to the last window of the previous
    /// column. When at the first window of the first column, wraps to the
    /// last window of the last column.
    pub fn focus_prev(&mut self) {
        if self.columns.is_empty() {
            return;
        }
        let start_col = self.focused_column;
        let start_win = self.focused_window_in_column;

        // Try moving up in the current column first
        if let Some(column) = self.columns.get(start_col) {
            let mut target = start_win;
            while target > 0 {
                target -= 1;
                if !self.minimized_windows.contains(&column.windows[target]) {
                    self.focused_window_in_column = target;
                    return;
                }
            }
        }

        // Move to previous columns, wrapping around
        let n = self.columns.len();
        for offset in 1..=n {
            let col_idx = (start_col + n - offset) % n;
            if let Some(column) = self.columns.get(col_idx) {
                // Find the last visible window in this column
                for i in (0..column.len()).rev() {
                    if !self.minimized_windows.contains(&column.windows[i]) {
                        self.focused_column = col_idx;
                        self.focused_window_in_column = i;
                        return;
                    }
                }
            }
        }
    }

    /// Clamp focus indices to valid column/window bounds.
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

    /// Check if a column has at least one non-minimized window.
    fn has_visible_window_in_column(&self, col_idx: usize) -> bool {
        self.columns.get(col_idx).is_some_and(|col| {
            col.windows
                .iter()
                .any(|w| !self.minimized_windows.contains(w))
        })
    }

    /// If the current `focused_window_in_column` points to a minimized window,
    /// shift it to the nearest non-minimized window in the same column.
    /// Searches downward first, then upward.
    fn adjust_focus_to_visible_in_column(&mut self) {
        let col = match self.columns.get(self.focused_column) {
            Some(c) => c,
            None => return,
        };
        let cur = self.focused_window_in_column;
        // Already pointing at a visible window — nothing to do
        if cur < col.len() && !self.minimized_windows.contains(&col.windows[cur]) {
            return;
        }
        // Search downward from current index
        for i in cur..col.len() {
            if !self.minimized_windows.contains(&col.windows[i]) {
                self.focused_window_in_column = i;
                return;
            }
        }
        // Search upward from current index
        for i in (0..cur).rev() {
            if !self.minimized_windows.contains(&col.windows[i]) {
                self.focused_window_in_column = i;
                return;
            }
        }
        // All minimized — leave index as is (has_visible_window_in_column
        // should have prevented us from landing here).
    }

    /// Get the currently focused window ID.
    pub fn focused_window(&self) -> Option<WindowId> {
        self.columns
            .get(self.focused_column)
            .and_then(|col| col.windows.get(self.focused_window_in_column))
            .copied()
    }

    /// Get the focused window ID, but only if it is not minimized.
    /// Falls back to the nearest non-minimized window in the focused column.
    /// Returns `None` if the workspace is empty or every window is minimized.
    pub fn focused_visible_window(&self) -> Option<WindowId> {
        let col = self.columns.get(self.focused_column)?;
        let cur = self.focused_window_in_column;

        // Check the exact focused index first
        if let Some(&wid) = col.windows.get(cur) {
            if !self.minimized_windows.contains(&wid) {
                return Some(wid);
            }
        }

        // Search downward then upward for a visible window
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

    /// Get the index of the currently focused column.
    pub fn focused_column_index(&self) -> usize {
        self.focused_column
    }

    /// Get the index of the focused window within the focused column.
    pub fn focused_window_index_in_column(&self) -> usize {
        self.focused_window_in_column
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
                return Ok(());
            }
        }
        Err(LayoutError::WindowNotFound(window_id))
    }
}
