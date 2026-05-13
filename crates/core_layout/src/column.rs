use serde::{Deserialize, Serialize};

use crate::types::{WindowId, MIN_COLUMN_WIDTH};

/// Display mode for a column.
///
/// Vertical (default) renders all non-minimized windows stacked top-to-bottom
/// using `height_weights`. Tabbed renders only the window at `active_idx`
/// filling the column rect; siblings are emitted as off-screen so the cloak
/// machinery hides them. A tab strip overlay (rendered by the daemon) lists
/// all tabs above the column.
///
/// Invariant when this column is the workspace's focused column:
///   `Tabbed { active_idx } => focused_window_in_column == active_idx`.
/// `Workspace::set_active_tab` is the canonical mutator that preserves it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ColumnMode {
    #[default]
    Vertical,
    /// `active_idx` is sticky across focus changes — when the column is
    /// unfocused, it remembers which tab was last on top.
    Tabbed { active_idx: usize },
}

/// A column in the infinite strip.
/// A column contains one or more vertically stacked windows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    /// Width of the column in pixels.
    pub(crate) width: i32,
    /// Windows in this column (vertically stacked).
    pub(crate) windows: Vec<WindowId>,
    /// Per-window height weights (parallel to `windows`, sums to ~1.0).
    /// Empty vec means equal distribution (backward compat).
    #[serde(default)]
    pub(crate) height_weights: Vec<f64>,
    /// Display mode (Vertical or Tabbed).
    #[serde(default)]
    pub(crate) mode: ColumnMode,
}

impl Column {
    /// Create a new column with a single window.
    /// Width is clamped to MIN_COLUMN_WIDTH (100px) minimum.
    pub fn new(window_id: WindowId, width: i32) -> Self {
        Self {
            width: width.max(MIN_COLUMN_WIDTH),
            windows: vec![window_id],
            height_weights: vec![1.0],
            mode: ColumnMode::Vertical,
        }
    }

    /// Create an empty column with specified width.
    /// Width is clamped to MIN_COLUMN_WIDTH (100px) minimum.
    pub fn empty(width: i32) -> Self {
        Self {
            width: width.max(MIN_COLUMN_WIDTH),
            windows: Vec::new(),
            height_weights: Vec::new(),
            mode: ColumnMode::Vertical,
        }
    }

    /// Check if the column is empty.
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// Get the number of windows in this column.
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// Add a window to this column (at the bottom of the stack).
    /// Resets all height weights to equal distribution.
    /// Tabbed `active_idx` is preserved (insert is at the end, no shift needed).
    pub fn add_window(&mut self, window_id: WindowId) {
        self.windows.push(window_id);
        self.equalize_height_weights();
        // Append-at-end never disturbs an existing active_idx.
        self.maintain_mode_invariant();
    }

    /// Remove a window from this column.
    /// Returns the index of the removed window if found, None otherwise.
    /// Renormalizes remaining height weights and adjusts `mode.active_idx`.
    pub fn remove_window(&mut self, window_id: WindowId) -> Option<usize> {
        if let Some(pos) = self.windows.iter().position(|&w| w == window_id) {
            self.windows.remove(pos);
            if pos < self.height_weights.len() {
                self.height_weights.remove(pos);
            }
            self.normalize_height_weights();
            self.on_windows_removed_at(pos);
            Some(pos)
        } else {
            None
        }
    }

    /// Remove a window from this column by index.
    /// Returns the removed window ID if the index was valid.
    /// Used by Workspace operations that already have the index in hand
    /// (e.g. `move_window_left/right`, `expel_to_left/right`) so they don't
    /// need to do direct Vec surgery — `mode.active_idx` invariant is handled.
    pub fn remove_at_index(&mut self, index: usize) -> Option<WindowId> {
        if index >= self.windows.len() {
            return None;
        }
        let wid = self.windows.remove(index);
        if index < self.height_weights.len() {
            self.height_weights.remove(index);
        }
        self.equalize_height_weights();
        self.on_windows_removed_at(index);
        Some(wid)
    }

    /// Get the width of this column.
    pub fn width(&self) -> i32 {
        self.width
    }

    /// Set the width of this column.
    /// Width is clamped to MIN_COLUMN_WIDTH (100px) minimum.
    pub fn set_width(&mut self, width: i32) {
        self.width = width.max(MIN_COLUMN_WIDTH);
    }

    /// Get a slice of windows in this column.
    pub fn windows(&self) -> &[WindowId] {
        &self.windows
    }

    /// Get the height weights for this column.
    pub fn height_weights(&self) -> &[f64] {
        &self.height_weights
    }

    /// Get the column's display mode.
    pub fn mode(&self) -> &ColumnMode {
        &self.mode
    }

    /// Whether this column is in tabbed display mode.
    pub fn is_tabbed(&self) -> bool {
        matches!(self.mode, ColumnMode::Tabbed { .. })
    }

    /// Get the active tab index, or None if Vertical.
    pub fn active_tab_idx(&self) -> Option<usize> {
        match self.mode {
            ColumnMode::Tabbed { active_idx } => Some(active_idx),
            ColumnMode::Vertical => None,
        }
    }

    /// Pick the tab index that should actually render in Tabbed mode,
    /// given a `is_minimized` predicate. Returns the active tab if visible;
    /// otherwise the first non-minimized tab (so the column doesn't render
    /// blank when `active_idx` is stale on a minimized window). Returns
    /// `None` for Vertical columns or when every tab is minimized.
    ///
    /// Shared between `compute_non_fullscreen_placements` (renders the
    /// tab) and the daemon's `update_tab_strip` (positions the overlay)
    /// so both pick the same effective tab — without this, the daemon
    /// could hide the strip while the layout still showed a fallback tab.
    pub fn effective_visible_tab<F>(&self, is_minimized: F) -> Option<usize>
    where
        F: Fn(WindowId) -> bool,
    {
        let active = self.active_tab_idx()?;
        if let Some(&w) = self.windows.get(active) {
            if !is_minimized(w) {
                return Some(active);
            }
        }
        for (i, &w) in self.windows.iter().enumerate() {
            if !is_minimized(w) {
                return Some(i);
            }
        }
        None
    }

    /// Switch this column into Tabbed mode with `active_idx` clamped to
    /// the current windows length. No-op (stays Vertical) for empty columns.
    pub fn set_tabbed(&mut self, active_idx: usize) {
        if self.windows.is_empty() {
            return;
        }
        let clamped = active_idx.min(self.windows.len() - 1);
        self.mode = ColumnMode::Tabbed { active_idx: clamped };
    }

    /// Switch this column back to Vertical mode.
    pub fn set_vertical(&mut self) {
        self.mode = ColumnMode::Vertical;
    }

    /// Set the active tab to a specific index. Clamped to windows length.
    /// No-op if the column is not Tabbed or is empty.
    pub fn set_active_tab(&mut self, index: usize) {
        if self.windows.is_empty() {
            return;
        }
        if let ColumnMode::Tabbed { .. } = self.mode {
            let clamped = index.min(self.windows.len() - 1);
            self.mode = ColumnMode::Tabbed { active_idx: clamped };
        }
    }

    /// Cycle the active tab forward (`forward=true`) or backward, wrapping.
    /// Returns the new active index, or None if the column is not Tabbed
    /// or has fewer than 2 windows.
    pub fn cycle_active_tab(&mut self, forward: bool) -> Option<usize> {
        let active_idx = self.active_tab_idx()?;
        let n = self.windows.len();
        if n < 2 {
            return None;
        }
        let next = if forward {
            (active_idx + 1) % n
        } else {
            (active_idx + n - 1) % n
        };
        self.mode = ColumnMode::Tabbed { active_idx: next };
        Some(next)
    }

    /// Check if this column contains a specific window.
    pub fn contains(&self, window_id: WindowId) -> bool {
        self.windows.contains(&window_id)
    }

    /// Get a window by index.
    pub fn get(&self, index: usize) -> Option<WindowId> {
        self.windows.get(index).copied()
    }

    /// Insert a window at a specific index.
    /// Resets all height weights to equal distribution.
    /// Tabbed `active_idx` is shifted right if the insert is at or before it.
    pub fn insert_at(&mut self, index: usize, window_id: WindowId) {
        let clamped = index.min(self.windows.len());
        self.windows.insert(clamped, window_id);
        self.equalize_height_weights();
        self.on_window_inserted_at(clamped);
    }

    /// Swap two windows by index within this column.
    /// Also swaps their height weights.
    /// If the active tab is one of the swapped indices, `active_idx` is
    /// updated so the same window remains active.
    pub fn swap_windows(&mut self, a: usize, b: usize) {
        if a >= self.windows.len() || b >= self.windows.len() {
            return;
        }
        self.windows.swap(a, b);
        if a < self.height_weights.len() && b < self.height_weights.len() {
            self.height_weights.swap(a, b);
        }
        self.on_windows_swapped(a, b);
    }

    /// Set a single window's height weight, distributing the remainder
    /// equally among siblings. Each sibling keeps at least 5% weight.
    pub fn set_height_weight(&mut self, index: usize, weight: f64) {
        let n = self.windows.len();
        if n <= 1 || index >= n {
            return;
        }
        self.ensure_height_weights();

        let siblings = n - 1;
        let min_sibling = 0.05;
        // Clamp so siblings can each have at least min_sibling
        let max_weight = 1.0 - (siblings as f64 * min_sibling);
        let weight = weight.clamp(min_sibling, max_weight);

        self.height_weights[index] = weight;
        let remainder = 1.0 - weight;
        let per_sibling = remainder / siblings as f64;
        for i in 0..n {
            if i != index {
                self.height_weights[i] = per_sibling;
            }
        }
    }

    /// Reset all height weights to equal distribution.
    pub fn equalize_height_weights(&mut self) {
        let n = self.windows.len();
        if n == 0 {
            self.height_weights.clear();
        } else {
            self.height_weights = vec![1.0 / n as f64; n];
        }
    }

    /// Ensure height_weights vec matches windows length (backward compat).
    pub(crate) fn ensure_height_weights(&mut self) {
        if self.height_weights.len() != self.windows.len() {
            self.equalize_height_weights();
        }
    }

    /// Normalize height weights so they sum to 1.0.
    fn normalize_height_weights(&mut self) {
        if self.height_weights.is_empty() {
            return;
        }
        let sum: f64 = self.height_weights.iter().sum();
        if sum > 0.0 && (sum - 1.0).abs() > 1e-9 {
            for w in &mut self.height_weights {
                *w /= sum;
            }
        }
    }

    /// Handle the active_idx invariant after a window was removed at `idx`.
    ///
    /// - If active_idx > idx: shift left.
    /// - If active_idx == idx: pin to the next window (or last, if `idx`
    ///   was the last); if no windows left, fall back to Vertical.
    /// - If active_idx < idx: unchanged.
    ///
    /// Also auto-reverts to Vertical when the column drops to ≤1 window.
    fn on_windows_removed_at(&mut self, idx: usize) {
        if let ColumnMode::Tabbed { active_idx } = self.mode {
            if self.windows.is_empty() {
                self.mode = ColumnMode::Vertical;
                return;
            }
            let new_active = match active_idx.cmp(&idx) {
                std::cmp::Ordering::Greater => active_idx - 1,
                std::cmp::Ordering::Equal => active_idx.min(self.windows.len() - 1),
                std::cmp::Ordering::Less => active_idx,
            };
            self.mode = ColumnMode::Tabbed { active_idx: new_active };
        }
        self.maintain_mode_invariant();
    }

    /// Handle the active_idx invariant after a window was inserted at `idx`.
    /// If the insert is at or before active_idx, active_idx shifts right
    /// so the same window remains active.
    fn on_window_inserted_at(&mut self, idx: usize) {
        if let ColumnMode::Tabbed { active_idx } = self.mode {
            let new_active = if idx <= active_idx {
                (active_idx + 1).min(self.windows.len().saturating_sub(1))
            } else {
                active_idx
            };
            self.mode = ColumnMode::Tabbed { active_idx: new_active };
        }
        self.maintain_mode_invariant();
    }

    /// Handle the active_idx invariant after `swap_windows(a, b)`.
    /// If active_idx is one of {a, b}, it follows the active window to the
    /// other index. Otherwise unchanged.
    fn on_windows_swapped(&mut self, a: usize, b: usize) {
        if let ColumnMode::Tabbed { active_idx } = self.mode {
            let new_active = if active_idx == a {
                b
            } else if active_idx == b {
                a
            } else {
                active_idx
            };
            self.mode = ColumnMode::Tabbed { active_idx: new_active };
        }
        self.maintain_mode_invariant();
    }

    /// Auto-revert to Vertical when the column drops to ≤1 window.
    /// A 1-tab tabbed column is visually identical to Vertical; a 0-tab
    /// one is degenerate. Also clamps any out-of-range active_idx as a
    /// belt-and-suspenders safety net.
    fn maintain_mode_invariant(&mut self) {
        if let ColumnMode::Tabbed { active_idx } = self.mode {
            if self.windows.len() <= 1 {
                self.mode = ColumnMode::Vertical;
            } else if active_idx >= self.windows.len() {
                self.mode = ColumnMode::Tabbed {
                    active_idx: self.windows.len() - 1,
                };
            }
        }
    }
}
