use serde::{Deserialize, Serialize};

use crate::types::{WindowId, MIN_COLUMN_WIDTH};

/// In the focused column, a tabbed active index must match workspace focus; use
/// `Workspace::set_active_tab`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ColumnMode {
    #[default]
    Vertical,
    /// `active_idx` is sticky across focus changes — when the column is
    /// unfocused, it remembers which tab was last on top.
    Tabbed { active_idx: usize },
}

/// A column in the infinite horizontal strip, displayed vertically or as tabs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub(crate) width: i32,
    pub(crate) windows: Vec<WindowId>,
    /// Parallel to `windows`; an empty vector means equal distribution.
    #[serde(default)]
    pub(crate) height_weights: Vec<f64>,
    #[serde(default)]
    pub(crate) mode: ColumnMode,
    #[serde(skip)]
    pub(crate) width_fraction_cache: Option<WidthFractionCache>,
}

#[derive(Debug, Clone)]
pub(crate) struct WidthFractionCache {
    pub fraction: f64,
    pub width: i32,
    pub base: i32,
    pub gap: i32,
}

impl PartialEq for Column {
    fn eq(&self, other: &Self) -> bool {
        self.width == other.width
            && self.windows == other.windows
            && self.height_weights == other.height_weights
            && self.mode == other.mode
    }
}

impl Column {
    /// Creates a single-window column with width clamped to at least 100 pixels.
    pub fn new(window_id: WindowId, width: i32) -> Self {
        Self {
            width: width.max(MIN_COLUMN_WIDTH),
            windows: vec![window_id],
            height_weights: vec![1.0],
            mode: ColumnMode::Vertical,
            width_fraction_cache: None,
        }
    }

    /// Creates an empty column with width clamped to at least 100 pixels.
    pub fn empty(width: i32) -> Self {
        Self {
            width: width.max(MIN_COLUMN_WIDTH),
            windows: Vec::new(),
            height_weights: Vec::new(),
            mode: ColumnMode::Vertical,
            width_fraction_cache: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// Add a window to this column (at the bottom of the stack).
    /// Resets all height weights to equal distribution.
    /// Tabbed `active_idx` is preserved (insert is at the end, no shift needed).
    pub fn add_window(&mut self, window_id: WindowId) {
        self.windows.push(window_id);
        self.equalize_height_weights();
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

    /// Returns the removed window, equalizes height weights, and maintains the active-tab index;
    /// invalid indices return `None`.
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

    pub fn width(&self) -> i32 {
        self.width
    }

    /// Sets width in pixels, clamped to at least 100.
    pub fn set_width(&mut self, width: i32) {
        self.width = width.max(MIN_COLUMN_WIDTH);
    }

    pub fn windows(&self) -> &[WindowId] {
        &self.windows
    }

    pub fn height_weights(&self) -> &[f64] {
        &self.height_weights
    }

    pub fn mode(&self) -> &ColumnMode {
        &self.mode
    }

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

    /// Returns the active non-minimized tab or the first non-minimized fallback; returns `None` for
    /// Vertical or fully minimized columns.
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
        self.mode = ColumnMode::Tabbed {
            active_idx: clamped,
        };
    }

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
            self.mode = ColumnMode::Tabbed {
                active_idx: clamped,
            };
        }
    }

    pub fn contains(&self, window_id: WindowId) -> bool {
        self.windows.contains(&window_id)
    }

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
    /// equally among siblings. Each window keeps at least 5% weight, or 1/n
    /// for columns with more than 20 windows.
    pub fn set_height_weight(&mut self, index: usize, weight: f64) {
        let n = self.windows.len();
        if n <= 1 || index >= n {
            return;
        }
        self.ensure_height_weights();

        let siblings = n - 1;
        let min_sibling = 0.05_f64.min(1.0 / n as f64);
        let max_weight = (1.0 - (siblings as f64 * min_sibling)).max(min_sibling);
        let weight = weight.clamp(min_sibling, max_weight);

        self.height_weights[index] = weight;
        let remainder = 1.0 - weight;
        let per_sibling = remainder / siblings as f64;
        for (i, w) in self.height_weights.iter_mut().enumerate().take(n) {
            if i != index {
                *w = per_sibling;
            }
        }
    }

    pub fn equalize_height_weights(&mut self) {
        let n = self.windows.len();
        if n == 0 {
            self.height_weights.clear();
        } else {
            self.height_weights = vec![1.0 / n as f64; n];
        }
    }

    // Missing or mismatched weights mean equal distribution.
    pub(crate) fn ensure_height_weights(&mut self) {
        if self.height_weights.len() != self.windows.len() {
            self.equalize_height_weights();
        }
    }

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

    // Preserve the active window where possible; otherwise clamp to a remaining tab.
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
            self.mode = ColumnMode::Tabbed {
                active_idx: new_active,
            };
        }
        self.maintain_mode_invariant();
    }

    // Preserve the active window when insertion shifts its index.
    fn on_window_inserted_at(&mut self, idx: usize) {
        if let ColumnMode::Tabbed { active_idx } = self.mode {
            let new_active = if idx <= active_idx {
                (active_idx + 1).min(self.windows.len().saturating_sub(1))
            } else {
                active_idx
            };
            self.mode = ColumnMode::Tabbed {
                active_idx: new_active,
            };
        }
        self.maintain_mode_invariant();
    }

    // The active tab follows its window through swaps.
    fn on_windows_swapped(&mut self, a: usize, b: usize) {
        if let ColumnMode::Tabbed { active_idx } = self.mode {
            let new_active = if active_idx == a {
                b
            } else if active_idx == b {
                a
            } else {
                active_idx
            };
            self.mode = ColumnMode::Tabbed {
                active_idx: new_active,
            };
        }
        self.maintain_mode_invariant();
    }

    // Zero- and one-window columns need no tabbed presentation.
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
