pub mod focus;
pub mod layout;
pub mod operations;
pub mod sizing;
pub mod state;

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::animation::{Easing, DEFAULT_ANIMATION_DURATION_MS};
use crate::column::Column;
use crate::row::WorkspaceRow;
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
    /// Center only when the focused column is wider than the viewport (so it
    /// cannot fit otherwise); behave like `JustInView` for columns that fit.
    OnOverflow,
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
    /// Whether the window stays visible above a fullscreen window.
    #[serde(default)]
    pub pinned: bool,
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
///    Exception: `center_single_column` collapses the range to the centered
///    offset for one fitting active column; `center_past_edges` also allows
///    negative offsets. When `center_past_edges` is true, `scroll_offset` may be
///    negative (centering first column) or exceed `max_scroll` (last column).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "WorkspaceRepr")]
pub struct Workspace {
    /// Horizontal strip rows (ordered top to bottom). Always at least one row.
    pub(crate) rows: Vec<WorkspaceRow>,
    /// Index of the currently focused row.
    pub(crate) focused_row: usize,
    /// Fractional height shares for rows. Always same length as `rows`.
    pub(crate) row_shares: Vec<f64>,
    /// Gap between adjacent rows in pixels.
    pub(crate) row_gap: i32,
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
    /// Known minimum heights for windows that enforce a minimum size.
    /// Detected by the platform layer and fed back so the layout engine
    /// can allocate correct intra-column heights from the start.
    #[serde(skip)]
    pub(crate) window_min_heights: HashMap<WindowId, i32>,
    /// Windows whose min-size constraints are scheduled for clearing on the
    /// next apply_layout pass. Populated when column composition changes so
    /// stale per-sibling constraints learned under the old window count can't
    /// over-allocate; the actual removal is deferred so a timed-out / paused
    /// apply path cannot strand the column with cleared constraints.
    #[serde(skip)]
    pub(crate) pending_min_size_clears: HashSet<WindowId>,
    /// Origin info for windows floated via toggle_floating.
    /// Stores (left_neighbor, fallback_column, fallback_row) to restore position when unfloating.
    #[serde(skip)]
    pub(crate) float_origin_column: HashMap<WindowId, (Option<WindowId>, usize, usize)>,
    /// Snap scroll instantly instead of animating (Windows "Show animations" off).
    #[serde(skip)]
    pub(crate) reduce_motion: bool,
    /// Duration (ms) for scroll animations. Set by the daemon from
    /// `[animation].scroll_duration_ms`; defaults to the engine default.
    #[serde(skip)]
    pub(crate) scroll_duration_ms: u64,
    /// Easing curve for scroll animations. Set by the daemon from
    /// `[animation].easing`; defaults to cubic ease-out.
    #[serde(skip)]
    pub(crate) scroll_easing: Easing,
    /// Whether center-column can scroll past content edges.
    #[serde(skip)]
    pub(crate) center_past_edges: bool,
    #[serde(skip)]
    pub(crate) center_single_column: bool,
    /// Pixels reserved at the top of each Tabbed column for the tab strip
    /// overlay. The daemon sets this from `appearance.tab_strip_height` scaled
    /// by the focused monitor's DPI so the strip has room to render above
    /// the active tab. `0` (default, used in tests/headless) means no
    /// reservation — strip would overlap the active tab's top edge or sit
    /// off-screen above the work area.
    #[serde(skip)]
    pub(crate) tab_strip_reserve_px: i32,
}

/// Intermediate deserialization representation supporting both legacy flat
/// format (single row) and multi-row format.
#[derive(Deserialize)]
struct WorkspaceRepr {
    // Multi-row format
    rows: Option<Vec<WorkspaceRow>>,
    focused_row: Option<usize>,
    row_shares: Option<Vec<f64>>,
    #[serde(default = "default_row_gap_value")]
    row_gap: i32,

    // Legacy format
    columns: Option<Vec<Column>>,
    focused_column: Option<usize>,
    focused_window_in_column: Option<usize>,
    scroll_offset: Option<f64>,

    // Common fields
    #[serde(default = "default_gap_value")]
    gap: i32,
    #[serde(default = "default_outer_gap_value")]
    outer_gap_left: i32,
    #[serde(default = "default_outer_gap_value")]
    outer_gap_right: i32,
    #[serde(default = "default_outer_gap_value")]
    outer_gap_top: i32,
    #[serde(default = "default_outer_gap_value")]
    outer_gap_bottom: i32,
    #[serde(default = "default_column_width_value")]
    default_column_width: i32,
    #[serde(default)]
    centering_mode: CenteringMode,
    #[serde(default)]
    floating_windows: Vec<FloatingWindow>,
    #[serde(default)]
    fullscreen_window: Option<WindowId>,
    #[serde(default)]
    minimized_windows: HashSet<WindowId>,
}

impl From<WorkspaceRepr> for Workspace {
    fn from(repr: WorkspaceRepr) -> Self {
        let (rows, focused_row, row_shares, row_gap) = if let Some(mut r) = repr.rows {
            if r.is_empty() {
                r.push(WorkspaceRow::default());
            }
            let focused_row = repr.focused_row.unwrap_or(0).min(r.len().saturating_sub(1));
            let shares = repr.row_shares.unwrap_or_else(|| vec![1.0; r.len()]);
            (r, focused_row, shares, repr.row_gap)
        } else {
            // Legacy single-row format
            let row = WorkspaceRow {
                columns: repr.columns.unwrap_or_default(),
                focused_column: repr.focused_column.unwrap_or(0),
                focused_window_in_column: repr.focused_window_in_column.unwrap_or(0),
                scroll_offset: repr.scroll_offset.unwrap_or(0.0),
                active_animation: None,
                maximized_column: None,
            };
            (
                vec![row],
                0,
                vec![1.0],
                repr.row_gap,
            )
        };

        let mut ws = Workspace {
            rows,
            focused_row,
            row_shares,
            row_gap,
            gap: repr.gap,
            outer_gap_left: repr.outer_gap_left,
            outer_gap_right: repr.outer_gap_right,
            outer_gap_top: repr.outer_gap_top,
            outer_gap_bottom: repr.outer_gap_bottom,
            default_column_width: repr.default_column_width,
            centering_mode: repr.centering_mode,
            floating_windows: repr.floating_windows,
            fullscreen_window: repr.fullscreen_window,
            minimized_windows: repr.minimized_windows,
            ..Default::default()
        };
        ws.clamp_focus_indices();
        ws
    }
}

/// State saved when a column is maximized to fill the viewport width.
#[derive(Debug, Clone)]
pub struct MaximizedColumnState {
    /// The original column width before maximizing.
    pub original_width: i32,
    pub(crate) width_fraction_cache: Option<crate::column::WidthFractionCache>,
    /// Sentinel window ID used to relocate the column after index shifts.
    pub sentinel_window: WindowId,
}

impl std::ops::Deref for Workspace {
    type Target = WorkspaceRow;

    fn deref(&self) -> &Self::Target {
        let fr = self.focused_row.min(self.rows.len().saturating_sub(1));
        &self.rows[fr]
    }
}

impl std::ops::DerefMut for Workspace {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let fr = self.focused_row.min(self.rows.len().saturating_sub(1));
        &mut self.rows[fr]
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            rows: vec![WorkspaceRow::default()],
            focused_row: 0,
            row_shares: vec![1.0],
            row_gap: DEFAULT_ROW_GAP,
            gap: DEFAULT_GAP,
            outer_gap_left: DEFAULT_OUTER_GAP,
            outer_gap_right: DEFAULT_OUTER_GAP,
            outer_gap_top: DEFAULT_OUTER_GAP,
            outer_gap_bottom: DEFAULT_OUTER_GAP,
            default_column_width: DEFAULT_COLUMN_WIDTH,
            centering_mode: CenteringMode::default(),
            floating_windows: Vec::new(),
            fullscreen_window: None,
            minimized_windows: HashSet::new(),
            window_min_widths: HashMap::new(),
            window_min_heights: HashMap::new(),
            pending_min_size_clears: HashSet::new(),
            float_origin_column: HashMap::new(),
            reduce_motion: false,
            scroll_duration_ms: DEFAULT_ANIMATION_DURATION_MS,
            scroll_easing: Easing::default(),
            center_past_edges: false,
            center_single_column: false,
            tab_strip_reserve_px: 0,
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

    /// Check if the workspace is empty (all rows have no columns).
    pub fn is_empty(&self) -> bool {
        self.rows.iter().all(|r| r.is_empty())
    }

    /// Check if a specific row has no columns.
    pub fn is_row_empty(&self, row_idx: usize) -> bool {
        self.rows.get(row_idx).map(|r| r.is_empty()).unwrap_or(true)
    }

    /// Get the number of columns in the focused row.
    pub fn column_count(&self) -> usize {
        self.column_count_for_row(self.focused_row)
    }

    /// Get the number of columns in a specific row.
    pub fn column_count_for_row(&self, row_idx: usize) -> usize {
        self.rows.get(row_idx).map(|r| r.column_count()).unwrap_or(0)
    }

    /// Check if a window ID already exists in the workspace (tiled in any row or floating).
    pub fn contains_window(&self, window_id: WindowId) -> bool {
        self.rows
            .iter()
            .any(|r| r.columns.iter().any(|c| c.windows.contains(&window_id)))
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
            pinned: false,
        });
        Ok(())
    }

    /// Set whether a floating window stays visible above a fullscreen window.
    ///
    /// Returns true if the window was found, false otherwise.
    pub fn set_floating_pinned(&mut self, window_id: WindowId, pinned: bool) -> bool {
        if let Some(floating) = self.floating_windows.iter_mut().find(|f| f.id == window_id) {
            floating.pinned = pinned;
            true
        } else {
            false
        }
    }

    /// Remove a floating window from the workspace.
    ///
    /// Returns true if the window was found and removed, false otherwise.
    pub fn remove_floating(&mut self, window_id: WindowId) -> bool {
        if let Some(pos) = self.floating_windows.iter().position(|f| f.id == window_id) {
            self.floating_windows.remove(pos);
            self.window_min_widths.remove(&window_id);
            self.window_min_heights.remove(&window_id);
            self.pending_min_size_clears.remove(&window_id);
            self.float_origin_column.remove(&window_id);
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

    /// Get the total width of the focused row's strip (sum of all column widths + gaps).
    ///
    /// Note: Negative gaps are treated as zero for calculation purposes.
    pub fn total_width(&self) -> i32 {
        self.total_width_for_row(self.focused_row)
    }

    /// Get the total width of a specific row's strip.
    pub fn total_width_for_row(&self, row_idx: usize) -> i32 {
        let Some(row) = self.rows.get(row_idx) else {
            return 0;
        };

        // Only count columns that have at least one non-minimized window
        let active_columns: Vec<&Column> = row
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
            .map(|c| self.effective_column_width(c))
            .fold(0i32, |acc, w| acc.saturating_add(w));
        let gaps = gap.saturating_mul(active_columns.len().saturating_sub(1) as i32);

        column_widths.saturating_add(gaps)
    }

    /// Get the current scroll offset of the focused row.
    pub fn scroll_offset(&self) -> f64 {
        self.scroll_offset_for_row(self.focused_row)
    }

    /// Get the current scroll offset for a specific row.
    pub fn scroll_offset_for_row(&self, row_idx: usize) -> f64 {
        self.rows.get(row_idx).map(|r| r.scroll_offset).unwrap_or(0.0)
    }

    /// Set the scroll offset of the focused row.
    pub fn set_scroll_offset(&mut self, offset: f64) {
        self.set_scroll_offset_for_row(self.focused_row, offset);
    }

    /// Set the scroll offset for a specific row.
    pub fn set_scroll_offset_for_row(&mut self, row_idx: usize, offset: f64) {
        if let Some(row) = self.rows.get_mut(row_idx) {
            row.scroll_offset = offset;
        }
    }

    /// Get a slice of all columns in the focused row.
    pub fn columns(&self) -> &[Column] {
        self.columns_for_row(self.focused_row)
    }

    /// Get a slice of all columns in a specific row.
    pub fn columns_for_row(&self, row_idx: usize) -> &[Column] {
        self.rows
            .get(row_idx)
            .map(|r| r.columns.as_slice())
            .unwrap_or(&[])
    }

    /// Get a mutable slice of all columns in the focused row.
    pub fn columns_mut(&mut self) -> &mut [Column] {
        let fr = self.focused_row;
        self.columns_mut_for_row(fr)
    }

    /// Get a mutable slice of all columns in a specific row.
    pub fn columns_mut_for_row(&mut self, row_idx: usize) -> &mut [Column] {
        if let Some(r) = self.rows.get_mut(row_idx) {
            &mut r.columns
        } else {
            &mut []
        }
    }

    /// Get a column by index in the focused row (safe access).
    pub fn column(&self, index: usize) -> Option<&Column> {
        self.column_for_row(self.focused_row, index)
    }

    /// Get a column by index in a specific row (safe access).
    pub fn column_for_row(&self, row_idx: usize, index: usize) -> Option<&Column> {
        self.rows.get(row_idx).and_then(|r| r.column(index))
    }

    /// Get a mutable column by index in the focused row.
    pub fn column_mut(&mut self, index: usize) -> Option<&mut Column> {
        let fr = self.focused_row;
        self.column_mut_for_row(fr, index)
    }

    /// Get a mutable column by index in a specific row.
    pub fn column_mut_for_row(&mut self, row_idx: usize, index: usize) -> Option<&mut Column> {
        self.rows.get_mut(row_idx).and_then(|r| r.column_mut(index))
    }

    /// Find a window's location in the workspace.
    /// Returns (column_index, window_index_in_column) if found in any row.
    pub fn find_window_location(&self, window_id: WindowId) -> Option<(usize, usize)> {
        self.find_window_location_rc(window_id)
            .map(|(_, col, win)| (col, win))
    }

    /// Find a window's row, column, and window indices in the workspace.
    /// Returns (row_index, column_index, window_index_in_column) if found.
    pub fn find_window_location_rc(
        &self,
        window_id: WindowId,
    ) -> Option<(usize, usize, usize)> {
        for (row_idx, row) in self.rows.iter().enumerate() {
            for (col_idx, column) in row.columns.iter().enumerate() {
                if let Some(win_idx) = column.windows.iter().position(|&w| w == window_id) {
                    return Some((row_idx, col_idx, win_idx));
                }
            }
        }
        None
    }

    /// Get total window count across all rows and columns.
    pub fn window_count(&self) -> usize {
        self.rows
            .iter()
            .map(|r| r.columns.iter().map(|c| c.len()).sum::<usize>())
            .sum()
    }

    /// Get total window count in a specific row.
    pub fn window_count_for_row(&self, row_idx: usize) -> usize {
        self.rows
            .get(row_idx)
            .map(|r| r.columns.iter().map(|c| c.len()).sum::<usize>())
            .unwrap_or(0)
    }

    /// Index of the focused column in the focused row.
    pub fn focused_column_index(&self) -> usize {
        self.rows
            .get(self.focused_row)
            .map(|r| r.focused_column)
            .unwrap_or(0)
    }

    /// Index of the focused window within the focused column of the focused row.
    pub fn focused_window_in_column(&self) -> usize {
        self.rows
            .get(self.focused_row)
            .map(|r| r.focused_window_in_column)
            .unwrap_or(0)
    }

    /// Get the focused window in the focused row, if any.
    pub fn focused_window(&self) -> Option<WindowId> {
        self.rows
            .get(self.focused_row)
            .and_then(|r| r.focused_window())
    }

    /// Get all rows in the workspace.
    pub fn rows(&self) -> &[WorkspaceRow] {
        &self.rows
    }

    /// Get a row by index.
    pub fn row(&self, index: usize) -> Option<&WorkspaceRow> {
        self.rows.get(index)
    }

    /// Get a mutable row by index.
    pub fn row_mut(&mut self, index: usize) -> Option<&mut WorkspaceRow> {
        self.rows.get_mut(index)
    }

    /// Get the number of rows in the workspace (always >= 1).
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Index of the currently focused row.
    pub fn focused_row(&self) -> usize {
        self.focused_row
    }

    /// Set the focused row index (clamped to `< row_count()`).
    pub fn focus_row(&mut self, row_idx: usize) {
        if row_idx < self.rows.len() {
            self.land_focus_in_row(row_idx);
        }
    }

    /// Set the focused row index, returning an error if out of bounds.
    pub fn try_focus_row(&mut self, row_idx: usize) -> Result<(), LayoutError> {
        if row_idx >= self.rows.len() {
            return Err(LayoutError::RowOutOfBounds(
                row_idx,
                self.rows.len().saturating_sub(1),
            ));
        }
        self.land_focus_in_row(row_idx);
        Ok(())
    }

    /// Get fractional height shares for rows.
    pub fn row_shares(&self) -> &[f64] {
        &self.row_shares
    }

    /// Get the gap between rows in pixels.
    pub fn row_gap(&self) -> i32 {
        self.row_gap
    }

    /// Set the gap between rows in pixels. Value is clamped to >= 0.
    pub fn set_row_gap(&mut self, gap: i32) {
        self.row_gap = gap.max(0);
    }

    /// Reconcile row configuration with new height shares.
    ///
    /// If adding rows, new empty rows are appended.
    /// If reducing rows, columns from removed rows are merged in order
    /// into the last surviving row, preserving all windows.
    pub fn set_row_layout(&mut self, shares: &[f64]) {
        let shares: Vec<f64> = if shares.is_empty() {
            vec![1.0]
        } else {
            shares.iter().map(|&s| s.max(0.01)).collect()
        };
        let new_count = shares.len();
        let cur_count = self.rows.len();

        if new_count > cur_count {
            for _ in cur_count..new_count {
                self.rows.push(WorkspaceRow::default());
            }
        } else if new_count < cur_count {
            let target_row_idx = new_count - 1;
            let removed_rows: Vec<WorkspaceRow> = self.rows.drain(new_count..).collect();
            for mut removed in removed_rows {
                self.rows[target_row_idx].columns.append(&mut removed.columns);
            }
            let target_col_count = self.rows[target_row_idx].columns.len();
            if target_col_count > 0 {
                self.rows[target_row_idx].focused_column = self.rows[target_row_idx]
                    .focused_column
                    .min(target_col_count - 1);
                let win_count = self.rows[target_row_idx].columns
                    [self.rows[target_row_idx].focused_column]
                    .len();
                if win_count > 0 {
                    self.rows[target_row_idx].focused_window_in_column = self.rows[target_row_idx]
                        .focused_window_in_column
                        .min(win_count - 1);
                } else {
                    self.rows[target_row_idx].focused_window_in_column = 0;
                }
            } else {
                self.rows[target_row_idx].focused_column = 0;
                self.rows[target_row_idx].focused_window_in_column = 0;
            }
        }

        self.row_shares = shares;
        self.focused_row = self.focused_row.min(self.rows.len().saturating_sub(1));
    }

    /// Get all window IDs in this workspace (both tiled and floating across all rows).
    ///
    /// Useful for migrating windows when monitors are disconnected.
    pub fn all_window_ids(&self) -> Vec<WindowId> {
        let mut ids: Vec<WindowId> = self
            .rows
            .iter()
            .flat_map(|r| r.columns.iter().flat_map(|c| c.windows().iter().copied()))
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
    pub fn set_outer_gaps(&mut self, left: i32, right: i32, top: i32, bottom: i32) {
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

    /// Get the pixels reserved at the top of Tabbed columns for the tab strip overlay.
    pub fn tab_strip_reserve_px(&self) -> i32 {
        self.tab_strip_reserve_px
    }

    /// Set the pixels reserved at the top of Tabbed columns for the tab strip overlay.
    /// Value is clamped to >= 0. Vertical columns ignore this value.
    pub fn set_tab_strip_reserve_px(&mut self, px: i32) {
        self.tab_strip_reserve_px = px.max(0);
    }

    /// Get the centering mode for focus changes.
    pub fn centering_mode(&self) -> CenteringMode {
        self.centering_mode
    }

    /// Set the centering mode for focus changes.
    pub fn set_centering_mode(&mut self, mode: CenteringMode) {
        self.centering_mode = mode;
    }

    /// Get whether scroll animations are skipped.
    pub fn reduce_motion(&self) -> bool {
        self.reduce_motion
    }

    /// Set whether to skip scroll animations (snap instantly).
    pub fn set_reduce_motion(&mut self, reduce: bool) {
        self.reduce_motion = reduce;
    }

    /// Set scroll animation duration and easing (from `[animation]` config).
    pub fn set_scroll_animation(&mut self, duration_ms: u64, easing: Easing) {
        self.scroll_duration_ms = duration_ms;
        self.scroll_easing = easing;
    }

    /// Center the only active tiled column when it fits inside the viewport.
    pub fn set_center_single_column(&mut self, center: bool) {
        self.center_single_column = center;
    }

    pub fn center_single_column(&self) -> bool {
        self.center_single_column
    }

    /// Set whether center-column can scroll past content edges.
    pub fn set_center_past_edges(&mut self, allow: bool) {
        self.center_past_edges = allow;
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
