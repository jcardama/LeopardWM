pub mod focus;
pub mod layout;
pub mod operations;
pub mod sizing;
pub mod state;

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::animation::ScrollAnimation;
use crate::column::Column;
use crate::types::*;

/// Focus centering mode.
/// Determines how the viewport adjusts when focus changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CenteringMode {
    /// Center the focused column in the viewport.
    #[default]
    Center,
    /// Only scroll if the focused column would be outside the viewport.
    JustInView,
}

/// A floating window that is not part of the tiling layout.
///
/// Floating windows are positioned at absolute coordinates and always
/// remain visible (not scrolled with the workspace).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingWindow {
    /// The window identifier.
    pub id: WindowId,
    /// The position and size of the floating window.
    pub rect: Rect,
}

/// The scrollable workspace.
/// This is the core data structure representing the infinite horizontal strip.
///
/// # Invariants
///
/// The following invariants are maintained by all methods:
///
/// 1. **No duplicate windows:** Each `WindowId` appears at most once.
/// 2. **Valid focus:** If `columns` is empty, `focused_window()` returns `None`.
///    Otherwise, `focused_column < columns.len()` and
///    `focused_window_in_column < columns[focused_column].len()`.
/// 3. **Valid column widths:** All column widths are >= `MIN_COLUMN_WIDTH` (100px).
/// 4. **Valid scroll range:** `0.0 <= scroll_offset <= max_scroll` where
///    `max_scroll = (total_width() - viewport_width).max(0)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// Columns in the workspace, ordered left to right.
    pub(crate) columns: Vec<Column>,
    /// Index of the currently focused column.
    pub(crate) focused_column: usize,
    /// Index of the focused window within the focused column.
    pub(crate) focused_window_in_column: usize,
    /// Current scroll offset (x position of viewport's left edge on the strip).
    pub(crate) scroll_offset: f64,
    /// Gap between columns in pixels (always >= 0).
    pub(crate) gap: i32,
    /// Gap at the left edge of the viewport (always >= 0).
    #[serde(default = "default_outer_gap_value")]
    pub(crate) outer_gap_left: i32,
    /// Gap at the right edge of the viewport (always >= 0).
    #[serde(default = "default_outer_gap_value")]
    pub(crate) outer_gap_right: i32,
    /// Gap at the top edge of the viewport (always >= 0).
    #[serde(default = "default_outer_gap_value")]
    pub(crate) outer_gap_top: i32,
    /// Gap at the bottom edge of the viewport (always >= 0).
    #[serde(default = "default_outer_gap_value")]
    pub(crate) outer_gap_bottom: i32,
    /// Default width for new columns (always >= MIN_COLUMN_WIDTH).
    pub(crate) default_column_width: i32,
    /// Centering mode for focus changes.
    pub(crate) centering_mode: CenteringMode,
    /// Active scroll animation, if any.
    #[serde(skip)]
    pub(crate) active_animation: Option<ScrollAnimation>,
    /// Floating windows outside the tiling layout.
    #[serde(default)]
    pub(crate) floating_windows: Vec<FloatingWindow>,
    /// Window ID in fullscreen mode, if any.
    #[serde(default)]
    pub(crate) fullscreen_window: Option<WindowId>,
    /// Windows that are currently minimized (excluded from layout).
    #[serde(default)]
    pub(crate) minimized_windows: HashSet<WindowId>,
    /// Known minimum widths for windows that enforce a minimum size.
    /// Detected by the platform layer and fed back so the layout engine
    /// can allocate correct column widths from the start.
    #[serde(skip)]
    pub(crate) window_min_widths: HashMap<WindowId, i32>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            columns: Vec::new(),
            focused_column: 0,
            focused_window_in_column: 0,
            scroll_offset: 0.0,
            gap: DEFAULT_GAP,
            outer_gap_left: DEFAULT_OUTER_GAP,
            outer_gap_right: DEFAULT_OUTER_GAP,
            outer_gap_top: DEFAULT_OUTER_GAP,
            outer_gap_bottom: DEFAULT_OUTER_GAP,
            default_column_width: DEFAULT_COLUMN_WIDTH,
            centering_mode: CenteringMode::default(),
            active_animation: None,
            floating_windows: Vec::new(),
            fullscreen_window: None,
            minimized_windows: HashSet::new(),
            window_min_widths: HashMap::new(),
        }
    }
}

impl Workspace {
    /// Create a new empty workspace with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a workspace with uniform gap settings.
    /// Gap values are clamped to >= 0.
    pub fn with_gaps(gap: i32, outer_gap: i32) -> Self {
        let og = outer_gap.max(0);
        Self {
            gap: gap.max(0),
            outer_gap_left: og,
            outer_gap_right: og,
            outer_gap_top: og,
            outer_gap_bottom: og,
            ..Default::default()
        }
    }

    /// Create a workspace with per-side outer gap settings.
    /// Gap values are clamped to >= 0.
    pub fn with_directional_gaps(
        gap: i32,
        outer_gap_left: i32,
        outer_gap_right: i32,
        outer_gap_top: i32,
        outer_gap_bottom: i32,
    ) -> Self {
        Self {
            gap: gap.max(0),
            outer_gap_left: outer_gap_left.max(0),
            outer_gap_right: outer_gap_right.max(0),
            outer_gap_top: outer_gap_top.max(0),
            outer_gap_bottom: outer_gap_bottom.max(0),
            ..Default::default()
        }
    }

    /// Check if the workspace is empty.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Get the number of columns.
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Check if a window ID already exists in the workspace (tiled or floating).
    pub fn contains_window(&self, window_id: WindowId) -> bool {
        self.columns.iter().any(|c| c.windows.contains(&window_id))
            || self.floating_windows.iter().any(|f| f.id == window_id)
    }

    /// Check if a window is floating.
    pub fn is_floating(&self, window_id: WindowId) -> bool {
        self.floating_windows.iter().any(|f| f.id == window_id)
    }

    /// Get the number of floating windows.
    pub fn floating_count(&self) -> usize {
        self.floating_windows.len()
    }

    /// Add a floating window to the workspace.
    ///
    /// # Errors
    ///
    /// Returns `LayoutError::DuplicateWindow` if the window ID already exists.
    pub fn add_floating(&mut self, window_id: WindowId, rect: Rect) -> Result<(), LayoutError> {
        if self.contains_window(window_id) {
            return Err(LayoutError::DuplicateWindow(window_id));
        }

        self.floating_windows.push(FloatingWindow {
            id: window_id,
            rect,
        });
        Ok(())
    }

    /// Remove a floating window from the workspace.
    ///
    /// Returns true if the window was found and removed, false otherwise.
    pub fn remove_floating(&mut self, window_id: WindowId) -> bool {
        if let Some(pos) = self.floating_windows.iter().position(|f| f.id == window_id) {
            self.floating_windows.remove(pos);
            self.window_min_widths.remove(&window_id);
            if self.fullscreen_window == Some(window_id) {
                self.fullscreen_window = None;
            }
            true
        } else {
            false
        }
    }

    /// Update the position/size of a floating window.
    pub fn update_floating(&mut self, window_id: WindowId, rect: Rect) -> bool {
        if let Some(floating) = self.floating_windows.iter_mut().find(|f| f.id == window_id) {
            floating.rect = rect;
            true
        } else {
            false
        }
    }

    /// Get all floating windows.
    pub fn floating_windows(&self) -> &[FloatingWindow] {
        &self.floating_windows
    }

    /// Get the total width of the strip (sum of all column widths + gaps).
    ///
    /// Note: Negative gaps are treated as zero for calculation purposes.
    pub fn total_width(&self) -> i32 {
        // Only count columns that have at least one non-minimized window
        let active_columns: Vec<&Column> = self
            .columns
            .iter()
            .filter(|c| self.is_column_active(c))
            .collect();

        if active_columns.is_empty() {
            return 0;
        }

        // Defensively clamp gaps to >= 0 in case fields were set directly
        let gap = self.gap.max(0);

        // Strip width = columns + inter-column gaps only.
        // Outer gaps are viewport padding, not strip content.
        let column_widths: i32 = active_columns
            .iter()
            .map(|c| c.width)
            .fold(0i32, |acc, w| acc.saturating_add(w));
        let gaps = gap.saturating_mul(active_columns.len().saturating_sub(1) as i32);

        column_widths.saturating_add(gaps)
    }

    /// Get the current scroll offset.
    pub fn scroll_offset(&self) -> f64 {
        self.scroll_offset
    }

    /// Get a slice of all columns.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Get a column by index (safe access).
    pub fn column(&self, index: usize) -> Option<&Column> {
        self.columns.get(index)
    }

    /// Find a window's location in the workspace.
    /// Returns (column_index, window_index_in_column) if found.
    pub fn find_window_location(&self, window_id: WindowId) -> Option<(usize, usize)> {
        for (col_idx, column) in self.columns.iter().enumerate() {
            if let Some(win_idx) = column.windows.iter().position(|&w| w == window_id) {
                return Some((col_idx, win_idx));
            }
        }
        None
    }

    /// Get total window count across all columns.
    pub fn window_count(&self) -> usize {
        self.columns.iter().map(|c| c.len()).sum()
    }

    /// Get all window IDs in this workspace (both tiled and floating).
    ///
    /// Useful for migrating windows when monitors are disconnected.
    pub fn all_window_ids(&self) -> Vec<WindowId> {
        let mut ids: Vec<WindowId> = self
            .columns
            .iter()
            .flat_map(|c| c.windows().iter().copied())
            .collect();
        ids.extend(self.floating_windows.iter().map(|f| f.id));
        ids
    }

    /// Get the gap between columns in pixels.
    pub fn gap(&self) -> i32 {
        self.gap
    }

    /// Set the gap between columns in pixels.
    /// Value is clamped to >= 0.
    pub fn set_gap(&mut self, gap: i32) {
        self.gap = gap.max(0);
    }

    /// Get outer gaps as (left, right, top, bottom).
    pub fn outer_gaps(&self) -> (i32, i32, i32, i32) {
        (
            self.outer_gap_left,
            self.outer_gap_right,
            self.outer_gap_top,
            self.outer_gap_bottom,
        )
    }

    /// Set the gap at viewport edges in pixels.
    /// Values are clamped to >= 0.
    pub fn set_outer_gaps(
        &mut self,
        left: i32,
        right: i32,
        top: i32,
        bottom: i32,
    ) {
        self.outer_gap_left = left.max(0);
        self.outer_gap_right = right.max(0);
        self.outer_gap_top = top.max(0);
        self.outer_gap_bottom = bottom.max(0);
    }

    /// Get the default width for new columns.
    pub fn default_column_width(&self) -> i32 {
        self.default_column_width
    }

    /// Set the default width for new columns.
    /// Value is clamped to >= MIN_COLUMN_WIDTH (100px).
    pub fn set_default_column_width(&mut self, width: i32) {
        self.default_column_width = width.max(MIN_COLUMN_WIDTH);
    }

    /// Get the centering mode for focus changes.
    pub fn centering_mode(&self) -> CenteringMode {
        self.centering_mode
    }

    /// Set the centering mode for focus changes.
    pub fn set_centering_mode(&mut self, mode: CenteringMode) {
        self.centering_mode = mode;
    }

    /// Calculate the x-coordinate of a column's left edge on the strip.
    ///
    /// Note: Negative gaps are treated as zero for calculation purposes.
    /// Check if a column has at least one non-minimized window.
    pub(crate) fn is_column_active(&self, column: &Column) -> bool {
        column
            .windows()
            .iter()
            .any(|w| !self.minimized_windows.contains(w))
    }
}
