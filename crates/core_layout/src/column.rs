use serde::{Deserialize, Serialize};

use crate::types::{WindowId, MIN_COLUMN_WIDTH};

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
}

impl Column {
    /// Create a new column with a single window.
    /// Width is clamped to MIN_COLUMN_WIDTH (100px) minimum.
    pub fn new(window_id: WindowId, width: i32) -> Self {
        Self {
            width: width.max(MIN_COLUMN_WIDTH),
            windows: vec![window_id],
            height_weights: vec![1.0],
        }
    }

    /// Create an empty column with specified width.
    /// Width is clamped to MIN_COLUMN_WIDTH (100px) minimum.
    pub fn empty(width: i32) -> Self {
        Self {
            width: width.max(MIN_COLUMN_WIDTH),
            windows: Vec::new(),
            height_weights: Vec::new(),
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
    pub fn add_window(&mut self, window_id: WindowId) {
        self.windows.push(window_id);
        self.equalize_height_weights();
    }

    /// Remove a window from this column.
    /// Returns the index of the removed window if found, None otherwise.
    /// Renormalizes remaining height weights.
    pub fn remove_window(&mut self, window_id: WindowId) -> Option<usize> {
        if let Some(pos) = self.windows.iter().position(|&w| w == window_id) {
            self.windows.remove(pos);
            if pos < self.height_weights.len() {
                self.height_weights.remove(pos);
            }
            self.normalize_height_weights();
            Some(pos)
        } else {
            None
        }
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
    pub fn insert_at(&mut self, index: usize, window_id: WindowId) {
        self.windows.insert(index, window_id);
        self.equalize_height_weights();
    }

    /// Swap two windows by index within this column.
    /// Also swaps their height weights.
    pub fn swap_windows(&mut self, a: usize, b: usize) {
        self.windows.swap(a, b);
        if a < self.height_weights.len() && b < self.height_weights.len() {
            self.height_weights.swap(a, b);
        }
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
}
