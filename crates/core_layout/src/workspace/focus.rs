use crate::*;

use crate::workspace::Workspace;

impl Workspace {
    /// Insert a new window as a new column to the right of the focused column
    /// in the focused row. Column width is clamped to MIN_COLUMN_WIDTH (100px) minimum.
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::DuplicateWindow` if the window ID already exists.
    pub fn insert_window(
        &mut self,
        window_id: WindowId,
        width: Option<i32>,
    ) -> Result<(), LayoutError> {
        let fr = self.focused_row;
        self.insert_window_in_row(window_id, fr, width)
    }

    /// Insert a new window as a new column in a specific row, to the right of
    /// that row's focused column (or at 0 if empty).
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::DuplicateWindow` if the window ID already exists.
    pub fn insert_window_in_row(
        &mut self,
        window_id: WindowId,
        row_idx: usize,
        width: Option<i32>,
    ) -> Result<(), LayoutError> {
        if self.contains_window(window_id) {
            return Err(LayoutError::DuplicateWindow(window_id));
        }

        let target_row = row_idx.min(self.rows.len().saturating_sub(1));
        let column_width = width
            .unwrap_or(self.default_column_width)
            .max(MIN_COLUMN_WIDTH);
        let new_column = Column::new(window_id, column_width);

        let row = &mut self.rows[target_row];
        if row.columns.is_empty() {
            row.columns.push(new_column);
            row.focused_column = 0;
        } else {
            let insert_pos = (row.focused_column + 1).min(row.columns.len());
            row.columns.insert(insert_pos, new_column);
            row.focused_column = insert_pos;
        }
        row.focused_window_in_column = 0;
        self.focused_row = target_row;

        debug_assert!(
            self.rows[target_row].focused_column < self.rows[target_row].columns.len(),
            "Invariant violation: focused_column out of bounds after insert"
        );

        Ok(())
    }

    /// Insert a new window as a new column in a specific row without changing
    /// current workspace focus.
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::DuplicateWindow` if the window ID already exists.
    pub fn insert_window_in_row_no_focus(
        &mut self,
        window_id: WindowId,
        row_idx: usize,
        width: Option<i32>,
    ) -> Result<(), LayoutError> {
        let saved_row = self.focused_row;
        let target_row = row_idx.min(self.rows.len().saturating_sub(1));
        let saved_col = self.rows[target_row].focused_column;
        let saved_win = self.rows[target_row].focused_window_in_column;

        self.insert_window_in_row(window_id, row_idx, width)?;

        self.focused_row = saved_row;
        let row = &mut self.rows[target_row];
        if saved_col < row.columns.len() && row.columns.len() > 1 {
            row.focused_column = saved_col;
            row.focused_window_in_column = saved_win;
        }

        Ok(())
    }

    /// Insert a window as a new single-window column at `index` in the focused row
    /// (clamped to the column count), then focus it.
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
        let fr = self.focused_row;
        self.insert_window_at_row_column(window_id, fr, width, index)
    }

    /// Insert a window as a new single-window column at `index` in a specific row.
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::DuplicateWindow` if the window ID already exists.
    pub fn insert_window_at_row_column(
        &mut self,
        window_id: WindowId,
        row_idx: usize,
        width: Option<i32>,
        index: usize,
    ) -> Result<(), LayoutError> {
        if self.contains_window(window_id) {
            return Err(LayoutError::DuplicateWindow(window_id));
        }
        let target_row = row_idx.min(self.rows.len().saturating_sub(1));
        let column_width = width
            .unwrap_or(self.default_column_width)
            .max(MIN_COLUMN_WIDTH);
        self.insert_column_at_row(Column::new(window_id, column_width), target_row, index);
        let row = &mut self.rows[target_row];
        let clamped = index.min(row.columns.len().saturating_sub(1));
        row.focused_column = clamped;
        row.focused_window_in_column = 0;
        self.focused_row = target_row;
        Ok(())
    }

    /// Append a window as a new single-window column at the END of the focused row's
    /// strip without changing current focus.
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::DuplicateWindow` if the window ID already exists.
    pub fn append_window_no_focus(
        &mut self,
        window_id: WindowId,
        width: Option<i32>,
    ) -> Result<(), LayoutError> {
        let fr = self.focused_row;
        let saved_col = self.rows[fr].focused_column;
        let saved_win = self.rows[fr].focused_window_in_column;
        let was_empty = self.rows[fr].columns.is_empty();
        let end_idx = self.rows[fr].columns.len();
        self.insert_window_at_row_column(window_id, fr, width, end_idx)?;
        if !was_empty {
            self.rows[fr].focused_column = saved_col;
            self.rows[fr].focused_window_in_column = saved_win;
        }
        Ok(())
    }

    /// Insert a window as a new column at `index` (clamped) in the focused row
    /// without moving the focused column; later columns shift right.
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
        let fr = self.focused_row;
        self.insert_window_at_row_column_no_focus(window_id, fr, width, index)
    }

    /// Insert a window as a new column at `index` (clamped) in a specific row
    /// without moving the focused column; later columns shift right.
    pub fn insert_window_at_row_column_no_focus(
        &mut self,
        window_id: WindowId,
        row_idx: usize,
        width: Option<i32>,
        index: usize,
    ) -> Result<(), LayoutError> {
        if self.contains_window(window_id) {
            return Err(LayoutError::DuplicateWindow(window_id));
        }
        let target_row = row_idx.min(self.rows.len().saturating_sub(1));
        let column_width = width
            .unwrap_or(self.default_column_width)
            .max(MIN_COLUMN_WIDTH);
        self.insert_column_at_row(Column::new(window_id, column_width), target_row, index);
        Ok(())
    }

    /// Insert a window into the focused row without changing the current focus.
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::DuplicateWindow` if the window ID already exists.
    pub fn insert_window_no_focus(
        &mut self,
        window_id: WindowId,
        width: Option<i32>,
    ) -> Result<(), LayoutError> {
        let fr = self.focused_row;
        let saved_col = self.rows[fr].focused_column;
        let saved_win = self.rows[fr].focused_window_in_column;

        self.insert_window(window_id, width)?;

        let row = &mut self.rows[fr];
        if saved_col < row.columns.len() && row.columns.len() > 1 {
            row.focused_column = saved_col;
            row.focused_window_in_column = saved_win;
        }

        Ok(())
    }

    /// Insert a window into an existing column (stacking) in the focused row.
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
        let fr = self.focused_row;
        self.insert_window_in_row_column(window_id, fr, column_index)
    }

    /// Insert a window into an existing column in a specific row.
    pub fn insert_window_in_row_column(
        &mut self,
        window_id: WindowId,
        row_idx: usize,
        column_index: usize,
    ) -> Result<(), LayoutError> {
        if self.contains_window(window_id) {
            return Err(LayoutError::DuplicateWindow(window_id));
        }

        let Some(row) = self.rows.get_mut(row_idx) else {
            return Err(LayoutError::RowOutOfBounds(row_idx, self.rows.len().saturating_sub(1)));
        };

        if column_index >= row.columns.len() {
            return Err(LayoutError::ColumnOutOfBounds(
                column_index,
                row.columns.len().saturating_sub(1),
            ));
        }

        for &wid in row.columns[column_index].windows() {
            self.pending_min_size_clears.insert(wid);
        }

        row.columns[column_index].add_window(window_id);
        Ok(())
    }

    /// Insert a window at a specific position within a column of the focused row.
    pub fn insert_window_in_column_at(
        &mut self,
        window_id: WindowId,
        column_index: usize,
        window_index: usize,
    ) -> Result<(), LayoutError> {
        let fr = self.focused_row;
        self.insert_window_in_row_column_at(window_id, fr, column_index, window_index)
    }

    /// Insert a window at a specific position within a column of a specific row.
    pub fn insert_window_in_row_column_at(
        &mut self,
        window_id: WindowId,
        row_idx: usize,
        column_index: usize,
        window_index: usize,
    ) -> Result<(), LayoutError> {
        if self.contains_window(window_id) {
            return Err(LayoutError::DuplicateWindow(window_id));
        }

        let target_row = row_idx.min(self.rows.len().saturating_sub(1));
        let Some(row) = self.rows.get_mut(target_row) else {
            return Err(LayoutError::RowOutOfBounds(
                target_row,
                self.rows.len().saturating_sub(1),
            ));
        };
        if column_index >= row.columns.len() {
            return Err(LayoutError::ColumnOutOfBounds(
                column_index,
                row.columns.len().saturating_sub(1),
            ));
        }

        for &wid in row.columns[column_index].windows() {
            self.pending_min_size_clears.insert(wid);
        }

        let clamped = window_index.min(row.columns[column_index].len());
        row.columns[column_index].insert_at(clamped, window_id);
        Ok(())
    }

    /// Remove a window from the workspace (searches all rows).
    /// If removing the last window from a column, the column is removed.
    pub fn remove_window(&mut self, window_id: WindowId) -> Result<(), LayoutError> {
        for row in self.rows.iter_mut() {
            for (col_idx, column) in row.columns.iter_mut().enumerate() {
                if let Some(removed_idx) = column.remove_window(window_id) {
                    self.minimized_windows.remove(&window_id);
                    self.window_min_widths.remove(&window_id);
                    self.window_min_heights.remove(&window_id);
                    self.pending_min_size_clears.remove(&window_id);
                    if self.fullscreen_window == Some(window_id) {
                        self.fullscreen_window = None;
                    }
                    if row
                        .maximized_column
                        .as_ref()
                        .is_some_and(|m| m.sentinel_window == window_id)
                    {
                        row.maximized_column = None;
                    }

                    if !column.is_empty() {
                        for &sibling in column.windows() {
                            self.pending_min_size_clears.insert(sibling);
                        }
                    }

                    if column.is_empty() {
                        row.columns.remove(col_idx);
                        if row.columns.is_empty() {
                            row.focused_column = 0;
                            row.focused_window_in_column = 0;
                            row.scroll_offset = 0.0;
                        } else if row.focused_column >= row.columns.len() {
                            row.focused_column = row.columns.len() - 1;
                        } else if row.focused_column > col_idx {
                            row.focused_column -= 1;
                        }
                    } else if col_idx == row.focused_column {
                        let col_len = row.columns[row.focused_column].len();
                        match removed_idx.cmp(&row.focused_window_in_column) {
                            std::cmp::Ordering::Less => {
                                row.focused_window_in_column -= 1;
                            }
                            std::cmp::Ordering::Equal => {
                                if row.focused_window_in_column >= col_len {
                                    row.focused_window_in_column = col_len.saturating_sub(1);
                                }
                            }
                            std::cmp::Ordering::Greater => {}
                        }
                    }

                    self.clamp_focus_indices();
                    return Ok(());
                }
            }
        }
        Err(LayoutError::WindowNotFound(window_id))
    }

    /// Move focus to the column on the left in the focused row.
    pub fn focus_left(&mut self) {
        let fr = self.focused_row;
        let start = self.rows[fr].focused_column;
        while self.rows[fr].focused_column > 0 {
            self.rows[fr].focused_column -= 1;
            self.land_focus_in_current_column();
            if self.has_visible_window_in_column(self.rows[fr].focused_column) {
                self.adjust_focus_to_visible_in_column();
                break;
            }
        }
        if !self.has_visible_window_in_column(self.rows[fr].focused_column) {
            self.rows[fr].focused_column = start;
            self.land_focus_in_current_column();
        }

        self.clamp_focus_indices();
        if self.has_visible_window_in_column(self.rows[fr].focused_column) {
            self.adjust_focus_to_visible_in_column();
        }
        self.sync_active_tab_to_focus();
    }

    /// Move focus to the column on the right in the focused row.
    pub fn focus_right(&mut self) {
        let fr = self.focused_row;
        let start = self.rows[fr].focused_column;
        while self.rows[fr].focused_column + 1 < self.rows[fr].columns.len() {
            self.rows[fr].focused_column += 1;
            self.land_focus_in_current_column();
            if self.has_visible_window_in_column(self.rows[fr].focused_column) {
                self.adjust_focus_to_visible_in_column();
                break;
            }
        }
        if !self.has_visible_window_in_column(self.rows[fr].focused_column) {
            self.rows[fr].focused_column = start;
            self.land_focus_in_current_column();
        }

        self.clamp_focus_indices();
        if self.has_visible_window_in_column(self.rows[fr].focused_column) {
            self.adjust_focus_to_visible_in_column();
        }
        self.sync_active_tab_to_focus();
    }

    /// Move focus to the first (leftmost) column with a visible window in the focused row.
    pub fn focus_start(&mut self) {
        self.focus_to_strip_end(false);
    }

    /// Move focus to the last (rightmost) column with a visible window in the focused row.
    pub fn focus_end(&mut self) {
        self.focus_to_strip_end(true);
    }

    fn focus_to_strip_end(&mut self, last: bool) {
        let fr = self.focused_row;
        if self.rows[fr].columns.is_empty() {
            return;
        }
        let start = self.rows[fr].focused_column;
        self.rows[fr].focused_column = if last {
            self.rows[fr].columns.len() - 1
        } else {
            0
        };
        while !self.has_visible_window_in_column(self.rows[fr].focused_column) {
            if last && self.rows[fr].focused_column > 0 {
                self.rows[fr].focused_column -= 1;
            } else if !last && self.rows[fr].focused_column + 1 < self.rows[fr].columns.len() {
                self.rows[fr].focused_column += 1;
            } else {
                self.rows[fr].focused_column = start;
                break;
            }
        }
        self.land_focus_in_current_column();
        self.clamp_focus_indices();
        if self.has_visible_window_in_column(self.rows[fr].focused_column) {
            self.adjust_focus_to_visible_in_column();
        }
        self.sync_active_tab_to_focus();
    }

    /// Focus a specific row and land focus safely on its last-focused window
    /// (or the first visible window if the remembered one is minimized, D2/D6).
    pub(crate) fn land_focus_in_row(&mut self, row_idx: usize) {
        if row_idx >= self.rows.len() {
            return;
        }
        self.focused_row = row_idx;
        self.clamp_focus_indices_for_row(row_idx);
        let row = &self.rows[row_idx];
        if !row.columns.is_empty() {
            if self.has_visible_window_in_column(row.focused_column) {
                self.adjust_focus_to_visible_in_column();
            } else {
                for c_idx in 0..row.columns.len() {
                    if self.has_visible_window_in_row_column(row_idx, c_idx) {
                        self.rows[row_idx].focused_column = c_idx;
                        self.adjust_focus_to_visible_in_column();
                        break;
                    }
                }
            }
        }
        self.sync_active_tab_to_focus();
    }

    /// Move focus to the window above in the current column.
    /// In multi-row layout, stepping past the top boundary moves focus to the row above (D2, D4).
    pub fn focus_up(&mut self) {
        let fr = self.focused_row;
        let Some(row) = self.rows.get(fr) else {
            return;
        };
        let Some(column) = row.columns.get(row.focused_column) else {
            if self.rows.len() > 1 && fr > 0 {
                self.land_focus_in_row(fr - 1);
            }
            return;
        };

        if column.is_tabbed() && column.len() >= 2 {
            if self.rows.len() == 1 {
                let n = column.len();
                let cur = row.focused_window_in_column;
                for offset in 1..=n {
                    let target = (cur + n - offset) % n;
                    if !self.minimized_windows.contains(&column.windows[target]) {
                        self.rows[fr].focused_window_in_column = target;
                        self.sync_active_tab_to_focus();
                        return;
                    }
                }
            } else {
                let cur = row.focused_window_in_column;
                for target in (0..cur).rev() {
                    if !self.minimized_windows.contains(&column.windows[target]) {
                        self.rows[fr].focused_window_in_column = target;
                        self.sync_active_tab_to_focus();
                        return;
                    }
                }
                if fr > 0 {
                    self.land_focus_in_row(fr - 1);
                }
            }
            return;
        }

        let cur = row.focused_window_in_column;
        let mut target = cur;
        while target > 0 {
            target -= 1;
            if !self.minimized_windows.contains(&column.windows[target]) {
                self.rows[fr].focused_window_in_column = target;
                return;
            }
        }
        if self.rows.len() > 1 && fr > 0 {
            self.land_focus_in_row(fr - 1);
        }
    }

    /// Move focus to the window below in the current column.
    /// In multi-row layout, stepping past the bottom boundary moves focus to the row below (D2, D4).
    pub fn focus_down(&mut self) {
        let fr = self.focused_row;
        let Some(row) = self.rows.get(fr) else {
            return;
        };
        let Some(column) = row.columns.get(row.focused_column) else {
            if self.rows.len() > 1 && fr + 1 < self.rows.len() {
                self.land_focus_in_row(fr + 1);
            }
            return;
        };

        if column.is_tabbed() && column.len() >= 2 {
            if self.rows.len() == 1 {
                let n = column.len();
                let cur = row.focused_window_in_column;
                for offset in 1..=n {
                    let target = (cur + offset) % n;
                    if !self.minimized_windows.contains(&column.windows[target]) {
                        self.rows[fr].focused_window_in_column = target;
                        self.sync_active_tab_to_focus();
                        return;
                    }
                }
            } else {
                let cur = row.focused_window_in_column;
                for target in (cur + 1)..column.len() {
                    if !self.minimized_windows.contains(&column.windows[target]) {
                        self.rows[fr].focused_window_in_column = target;
                        self.sync_active_tab_to_focus();
                        return;
                    }
                }
                if fr + 1 < self.rows.len() {
                    self.land_focus_in_row(fr + 1);
                }
            }
            return;
        }

        let cur = row.focused_window_in_column;
        let mut target = cur;
        while target + 1 < column.len() {
            target += 1;
            if !self.minimized_windows.contains(&column.windows[target]) {
                self.rows[fr].focused_window_in_column = target;
                return;
            }
        }
        if self.rows.len() > 1 && fr + 1 < self.rows.len() {
            self.land_focus_in_row(fr + 1);
        }
    }

    /// Focus the row above unconditionally (lands on target row's last-focused window, D2/D6).
    pub fn focus_row_up(&mut self) {
        if self.focused_row > 0 {
            self.land_focus_in_row(self.focused_row - 1);
        }
    }

    /// Focus the row below unconditionally (lands on target row's last-focused window, D2/D6).
    pub fn focus_row_down(&mut self) {
        if self.focused_row + 1 < self.rows.len() {
            self.land_focus_in_row(self.focused_row + 1);
        }
    }

    /// Move a window to another row, inserting it as a new column at `target_col`
    /// (clamped to target row's column count).
    pub fn move_window_to_row_at_column(
        &mut self,
        window_id: WindowId,
        target_row: usize,
        target_col: usize,
    ) -> Result<(), LayoutError> {
        if target_row >= self.rows.len() {
            return Err(LayoutError::RowOutOfBounds(
                target_row,
                self.rows.len().saturating_sub(1),
            ));
        }
        let Some((source_row, source_col, source_win)) = self.find_window_location_rc(window_id)
        else {
            return Err(LayoutError::WindowNotFound(window_id));
        };
        if source_row == target_row {
            return Ok(());
        }

        let width = self.rows[source_row].columns[source_col].width();
        let cache = self.rows[source_row].columns[source_col]
            .width_fraction_cache
            .clone();

        if self.rows[source_row]
            .maximized_column
            .as_ref()
            .is_some_and(|m| m.sentinel_window == window_id)
        {
            self.rows[source_row].maximized_column = None;
        }

        let _ = self.rows[source_row].columns[source_col].remove_at_index(source_win);
        if self.rows[source_row].columns[source_col].is_empty() {
            self.rows[source_row].columns.remove(source_col);
            if self.rows[source_row].focused_column > source_col {
                self.rows[source_row].focused_column -= 1;
            }
        }
        self.clamp_focus_indices_for_row(source_row);

        let mut new_col = Column::new(window_id, width);
        new_col.width_fraction_cache = cache;

        let insert_idx = target_col.min(self.rows[target_row].columns.len());
        self.rows[target_row].columns.insert(insert_idx, new_col);
        self.rows[target_row].focused_column = insert_idx;
        self.rows[target_row].focused_window_in_column = 0;
        self.focused_row = target_row;
        self.sync_active_tab_to_focus();
        Ok(())
    }

    /// Move a window to another row, inserting it to the right of target row's focused column
    /// (or at 0 if empty).
    pub fn move_window_to_row(
        &mut self,
        window_id: WindowId,
        target_row: usize,
    ) -> Result<(), LayoutError> {
        if target_row >= self.rows.len() {
            return Err(LayoutError::RowOutOfBounds(
                target_row,
                self.rows.len().saturating_sub(1),
            ));
        }
        let insert_idx = if self.rows[target_row].columns.is_empty() {
            0
        } else {
            (self.rows[target_row].focused_column + 1).min(self.rows[target_row].columns.len())
        };
        self.move_window_to_row_at_column(window_id, target_row, insert_idx)
    }

    /// Move the currently focused window to the row above.
    pub fn move_window_to_row_up(&mut self) -> Result<(), LayoutError> {
        if self.focused_row > 0 {
            if let Some(wid) = self.focused_window() {
                let target = self.focused_row - 1;
                return self.move_window_to_row(wid, target);
            }
        }
        Ok(())
    }

    /// Move the currently focused window to the row below.
    pub fn move_window_to_row_down(&mut self) -> Result<(), LayoutError> {
        if self.focused_row + 1 < self.rows.len() {
            if let Some(wid) = self.focused_window() {
                let target = self.focused_row + 1;
                return self.move_window_to_row(wid, target);
            }
        }
        Ok(())
    }

    /// Move focus to the next window in row-major linear order across all rows.
    /// Skips empty rows.
    pub fn focus_next(&mut self) {
        if self.window_count() == 0 {
            return;
        }

        let fr = self.focused_row;
        let start_col = self.rows[fr].focused_column;
        let start_win = self.rows[fr].focused_window_in_column;

        // Try moving down in current column first
        if let Some(column) = self.rows[fr].columns.get(start_col) {
            let mut target = start_win;
            while target + 1 < column.len() {
                target += 1;
                if !self.minimized_windows.contains(&column.windows[target]) {
                    self.rows[fr].focused_window_in_column = target;
                    self.sync_active_tab_to_focus();
                    return;
                }
            }
        }

        // Try remaining columns in current row
        let col_count = self.rows[fr].columns.len();
        for col_idx in (start_col + 1)..col_count {
            if self.has_visible_window_in_row_column(fr, col_idx) {
                self.rows[fr].focused_column = col_idx;
                self.rows[fr].focused_window_in_column = 0;
                self.adjust_focus_to_visible_in_column();
                self.sync_active_tab_to_focus();
                return;
            }
        }

        // Advance to subsequent rows, wrapping around, skipping empty rows
        let row_count = self.rows.len();
        for r_offset in 1..=row_count {
            let r_idx = (fr + r_offset) % row_count;
            let r_cols = self.rows[r_idx].columns.len();
            let limit = if r_idx == fr { start_col + 1 } else { r_cols };
            for c_idx in 0..limit {
                if self.has_visible_window_in_row_column(r_idx, c_idx) {
                    self.focused_row = r_idx;
                    self.rows[r_idx].focused_column = c_idx;
                    self.rows[r_idx].focused_window_in_column = 0;
                    self.adjust_focus_to_visible_in_column();
                    self.sync_active_tab_to_focus();
                    return;
                }
            }
        }
    }

    /// Move focus to the previous window in row-major linear order across all rows.
    /// Skips empty rows.
    pub fn focus_prev(&mut self) {
        if self.window_count() == 0 {
            return;
        }

        let fr = self.focused_row;
        let start_col = self.rows[fr].focused_column;
        let start_win = self.rows[fr].focused_window_in_column;

        // Try moving up in current column first
        if let Some(column) = self.rows[fr].columns.get(start_col) {
            let mut target = start_win;
            while target > 0 {
                target -= 1;
                if !self.minimized_windows.contains(&column.windows[target]) {
                    self.rows[fr].focused_window_in_column = target;
                    self.sync_active_tab_to_focus();
                    return;
                }
            }
        }

        // Try previous columns in current row
        for col_idx in (0..start_col).rev() {
            if let Some(column) = self.rows[fr].columns.get(col_idx) {
                for i in (0..column.len()).rev() {
                    if !self.minimized_windows.contains(&column.windows[i]) {
                        self.rows[fr].focused_column = col_idx;
                        self.rows[fr].focused_window_in_column = i;
                        self.sync_active_tab_to_focus();
                        return;
                    }
                }
            }
        }

        // Backtrack to preceding rows, wrapping around, skipping empty rows
        let row_count = self.rows.len();
        for r_offset in 1..=row_count {
            let r_idx = (fr + row_count - r_offset) % row_count;
            let r_cols = self.rows[r_idx].columns.len();
            let start = if r_idx == fr { start_col } else { 0 };
            for c_idx in (start..r_cols).rev() {
                if let Some(column) = self.rows[r_idx].columns.get(c_idx) {
                    for i in (0..column.len()).rev() {
                        if !self.minimized_windows.contains(&column.windows[i]) {
                            self.focused_row = r_idx;
                            self.rows[r_idx].focused_column = c_idx;
                            self.rows[r_idx].focused_window_in_column = i;
                            self.sync_active_tab_to_focus();
                            return;
                        }
                    }
                }
            }
        }
    }

    fn land_focus_in_current_column(&mut self) {
        let fr = self.focused_row;
        let Some(col) = self.rows[fr].columns.get(self.rows[fr].focused_column) else {
            return;
        };
        match col.mode {
            ColumnMode::Vertical => {
                let col_len = col.len();
                if self.rows[fr].focused_window_in_column >= col_len {
                    self.rows[fr].focused_window_in_column = col_len.saturating_sub(1);
                }
            }
            ColumnMode::Tabbed { active_idx } => {
                self.rows[fr].focused_window_in_column = active_idx;
            }
        }
    }

    pub(crate) fn sync_active_tab_to_focus(&mut self) {
        let fr = self.focused_row;
        let idx = self.rows[fr].focused_window_in_column;
        let col_idx = self.rows[fr].focused_column;
        if let Some(col) = self.rows[fr].columns.get_mut(col_idx) {
            col.set_active_tab(idx);
        }
    }

    pub fn toggle_focused_column_tabbed_mode(&mut self) {
        let fr = self.focused_row;
        let cur_focus = self.rows[fr].focused_window_in_column;
        let col_idx = self.rows[fr].focused_column;
        let Some(col) = self.rows[fr].columns.get_mut(col_idx) else {
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

    pub fn set_active_tab(&mut self, column: usize, tab_idx: usize) -> Result<(), LayoutError> {
        let fr = self.focused_row;
        if column >= self.rows[fr].columns.len() {
            return Err(LayoutError::ColumnOutOfBounds(
                column,
                self.rows[fr].columns.len().saturating_sub(1),
            ));
        }
        let col = &mut self.rows[fr].columns[column];
        if !col.is_tabbed() {
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
        if column == self.rows[fr].focused_column {
            self.rows[fr].focused_window_in_column = tab_idx;
        }
        Ok(())
    }

    pub(crate) fn clamp_focus_indices(&mut self) {
        if self.rows.is_empty() {
            self.rows.push(WorkspaceRow::default());
            self.focused_row = 0;
            return;
        }
        if self.focused_row >= self.rows.len() {
            self.focused_row = self.rows.len() - 1;
        }
        for r_idx in 0..self.rows.len() {
            self.clamp_focus_indices_for_row(r_idx);
        }
    }

    pub(crate) fn clamp_focus_indices_for_row(&mut self, row_idx: usize) {
        let Some(row) = self.rows.get_mut(row_idx) else {
            return;
        };
        if row.columns.is_empty() {
            row.focused_column = 0;
            row.focused_window_in_column = 0;
            return;
        }
        if row.focused_column >= row.columns.len() {
            row.focused_column = row.columns.len() - 1;
        }
        let col_len = row.columns[row.focused_column].len();
        if col_len == 0 {
            row.focused_window_in_column = 0;
        } else if row.focused_window_in_column >= col_len {
            row.focused_window_in_column = col_len - 1;
        }
    }

    fn has_visible_window_in_column(&self, col_idx: usize) -> bool {
        self.has_visible_window_in_row_column(self.focused_row, col_idx)
    }

    fn has_visible_window_in_row_column(&self, row_idx: usize, col_idx: usize) -> bool {
        self.rows
            .get(row_idx)
            .and_then(|r| r.columns.get(col_idx))
            .is_some_and(|col| {
                col.windows
                    .iter()
                    .any(|w| !self.minimized_windows.contains(w))
            })
    }

    fn adjust_focus_to_visible_in_column(&mut self) {
        let fr = self.focused_row;
        let col_idx = self.rows[fr].focused_column;
        let col = match self.rows[fr].columns.get(col_idx) {
            Some(c) => c,
            None => return,
        };
        let cur = self.rows[fr].focused_window_in_column;
        if cur < col.len() && !self.minimized_windows.contains(&col.windows[cur]) {
            return;
        }
        for i in cur..col.len() {
            if !self.minimized_windows.contains(&col.windows[i]) {
                self.rows[fr].focused_window_in_column = i;
                return;
            }
        }
        for i in (0..cur).rev() {
            if !self.minimized_windows.contains(&col.windows[i]) {
                self.rows[fr].focused_window_in_column = i;
                return;
            }
        }
    }

    /// Get the focused window ID in the focused row, but only if not minimized.
    pub fn focused_visible_window(&self) -> Option<WindowId> {
        let fr = self.focused_row;
        let row = self.rows.get(fr)?;
        let col = row.columns.get(row.focused_column)?;
        let cur = row.focused_window_in_column;

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

    /// Get the index of the focused window within the focused column.
    pub fn focused_window_index_in_column(&self) -> usize {
        self.focused_window_in_column()
    }

    /// Whether the focused window is at the top of its column.
    pub fn at_column_top(&self) -> bool {
        let fr = self.focused_row;
        let Some(row) = self.rows.get(fr) else {
            return false;
        };
        match row.columns.get(row.focused_column) {
            Some(col) if col.is_empty() => false,
            Some(col) if col.is_tabbed() && col.len() >= 2 => {
                if self.rows.len() > 1 {
                    row.focused_window_in_column == 0
                } else {
                    false
                }
            }
            Some(_) => row.focused_window_in_column == 0,
            None => false,
        }
    }

    /// Whether the focused window is at the bottom of its column.
    pub fn at_column_bottom(&self) -> bool {
        let fr = self.focused_row;
        let Some(row) = self.rows.get(fr) else {
            return false;
        };
        match row.columns.get(row.focused_column) {
            Some(col) if col.is_empty() => false,
            Some(col) if col.is_tabbed() && col.len() >= 2 => {
                if self.rows.len() > 1 {
                    row.focused_window_in_column + 1 >= col.len()
                } else {
                    false
                }
            }
            Some(col) => row.focused_window_in_column + 1 >= col.len(),
            None => false,
        }
    }

    /// Whether the focused window is at the top of the entire workspace (row 0 and column top).
    pub fn at_workspace_top(&self) -> bool {
        self.focused_row == 0 && self.at_column_top()
    }

    /// Whether the focused window is at the bottom of the entire workspace (last row and column bottom).
    pub fn at_workspace_bottom(&self) -> bool {
        (self.focused_row + 1 >= self.rows.len()) && self.at_column_bottom()
    }

    /// Set focus to a specific column and window index within the focused row.
    pub fn set_focus(&mut self, column: usize, window_in_column: usize) -> Result<(), LayoutError> {
        let fr = self.focused_row;
        if column >= self.rows[fr].columns.len() {
            return Err(LayoutError::ColumnOutOfBounds(
                column,
                self.rows[fr].columns.len().saturating_sub(1),
            ));
        }

        let col_len = self.rows[fr].columns[column].len();
        if window_in_column >= col_len {
            return Err(LayoutError::WindowIndexOutOfBounds(
                window_in_column,
                column,
                col_len.saturating_sub(1),
            ));
        }

        self.rows[fr].focused_column = column;
        self.rows[fr].focused_window_in_column = window_in_column;
        self.sync_active_tab_to_focus();
        Ok(())
    }

    /// Focus a window by its ID (searches across all rows).
    pub fn focus_window(&mut self, window_id: WindowId) -> Result<(), LayoutError> {
        if let Some((row_idx, col_idx, win_idx)) = self.find_window_location_rc(window_id) {
            self.focused_row = row_idx;
            self.rows[row_idx].focused_column = col_idx;
            self.rows[row_idx].focused_window_in_column = win_idx;
            self.sync_active_tab_to_focus();
            return Ok(());
        }
        Err(LayoutError::WindowNotFound(window_id))
    }
}
