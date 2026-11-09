pub mod focus;
pub mod layout;
pub mod operations;
pub mod sizing;
pub mod state;

use crate::animation::{Easing, ScrollAnimation, DEFAULT_ANIMATION_DURATION_MS};
use crate::column::Column;
use crate::types::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Controls how the viewport adjusts when focus changes.
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

/// A window positioned independently of the tiled strip and its scroll offset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingWindow {
    pub id: WindowId,
    pub rect: Rect,
    /// Whether the window stays visible above a fullscreen window.
    #[serde(default)]
    pub pinned: bool,
}

/// A horizontal strip of tiled columns with separate floating windows.
/// Layout operations maintain unique windows, valid focus indices, and minimum column widths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub(crate) columns: Vec<Column>,
    pub(crate) focused_column: usize,
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
    pub(crate) centering_mode: CenteringMode,
    #[serde(skip)]
    pub(crate) active_animation: Option<ScrollAnimation>,
    #[serde(default)]
    pub(crate) floating_windows: Vec<FloatingWindow>,
    #[serde(default)]
    pub(crate) fullscreen_window: Option<WindowId>,
    /// Windows that are currently minimized (excluded from layout).
    #[serde(default)]
    pub(crate) minimized_windows: HashSet<WindowId>,
    /// Platform-detected minimum widths used to allocate column widths.
    #[serde(skip)]
    pub(crate) window_min_widths: HashMap<WindowId, i32>,
    /// Platform-detected minimum heights used to allocate intra-column heights.
    #[serde(skip)]
    pub(crate) window_min_heights: HashMap<WindowId, i32>,
    /// Defer clearing composition-dependent constraints until apply-time so paused or timed-out
    /// layouts retain them.
    #[serde(skip)]
    pub(crate) pending_min_size_clears: HashSet<WindowId>,
    /// Stores (left_neighbor, fallback_index) for restoring an unfloated window's position.
    /// Prefer the surviving neighbor after column changes; otherwise use the fallback index.
    #[serde(skip)]
    pub(crate) float_origin_column: HashMap<WindowId, (Option<WindowId>, usize)>,
    /// Snap scroll instantly instead of animating (Windows "Show animations" off).
    #[serde(skip)]
    pub(crate) reduce_motion: bool,
    /// Scroll duration in milliseconds, supplied by the daemon's `[animation].scroll_duration_ms`.
    #[serde(skip)]
    pub(crate) scroll_duration_ms: u64,
    /// Scroll easing supplied by the daemon's `[animation].easing`; defaults to cubic ease-out.
    #[serde(skip)]
    pub(crate) scroll_easing: Easing,
    /// Lets centering scroll to negative offsets or past the maximum scroll.
    #[serde(skip)]
    pub(crate) center_past_edges: bool,
    /// Outside fullscreen, pins scroll to center when exactly one active column fits.
    #[serde(skip)]
    pub(crate) center_single_column: bool,
    #[serde(skip)]
    pub(crate) maximized_column: Option<MaximizedColumnState>,
    /// DPI-scaled tab-strip space supplied by the daemon from `appearance.tab_strip_height`.
    /// Zero reserves no space, allowing the overlay to overlap the active tab or extend above the
    /// work area.
    #[serde(skip)]
    pub(crate) tab_strip_reserve_px: i32,
}

/// State saved when a column is maximized to fill the viewport width.
#[derive(Debug, Clone)]
pub struct MaximizedColumnState {
    pub original_width: i32,
    pub(crate) width_fraction_cache: Option<crate::column::WidthFractionCache>,
    /// Sentinel window ID used to relocate the column after index shifts.
    pub sentinel_window: WindowId,
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
            window_min_heights: HashMap::new(),
            pending_min_size_clears: HashSet::new(),
            float_origin_column: HashMap::new(),
            reduce_motion: false,
            scroll_duration_ms: DEFAULT_ANIMATION_DURATION_MS,
            scroll_easing: Easing::default(),
            center_past_edges: false,
            center_single_column: false,
            maximized_column: None,
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

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Check if a window ID already exists in the workspace (tiled or floating).
    pub fn contains_window(&self, window_id: WindowId) -> bool {
        self.columns.iter().any(|c| c.windows.contains(&window_id))
            || self.floating_windows.iter().any(|f| f.id == window_id)
    }

    pub fn is_floating(&self, window_id: WindowId) -> bool {
        self.floating_windows.iter().any(|f| f.id == window_id)
    }

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

    pub fn update_floating(&mut self, window_id: WindowId, rect: Rect) -> bool {
        if let Some(floating) = self.floating_windows.iter_mut().find(|f| f.id == window_id) {
            floating.rect = rect;
            true
        } else {
            false
        }
    }

    /// Changes a floating window's width by `delta`, keeping its top-left corner and height.
    /// Shrinking stops at `MIN_COLUMN_WIDTH` but never widens a window that is already narrower.
    ///
    /// Returns true if the window was found, false otherwise.
    pub fn resize_floating(&mut self, window_id: WindowId, delta: i32) -> bool {
        if let Some(floating) = self.floating_windows.iter_mut().find(|f| f.id == window_id) {
            let width = floating.rect.width;
            let floor = MIN_COLUMN_WIDTH.min(width);
            floating.rect.width = width.saturating_add(delta).max(floor);
            true
        } else {
            false
        }
    }

    fn change_floating_rect(
        &mut self,
        window_id: WindowId,
        area: Rect,
        change: impl FnOnce(&mut Rect),
    ) {
        if let Some(window) = self.floating_windows.iter_mut().find(|f| f.id == window_id) {
            let rect = &mut window.rect;
            change(rect);
            rect.x = rect.x.clamp(
                area.x,
                area.x
                    .saturating_add(area.width.saturating_sub(rect.width).max(0)),
            );
            rect.y = rect.y.clamp(
                area.y,
                area.y
                    .saturating_add(area.height.saturating_sub(rect.height).max(0)),
            );
        }
    }

    /// Move a floating window by physical pixels and constrain it to the work area.
    pub fn move_floating(&mut self, window_id: WindowId, dx: i32, dy: i32, area: Rect) {
        self.change_floating_rect(window_id, area, |rect| {
            rect.x = rect.x.saturating_add(dx);
            rect.y = rect.y.saturating_add(dy);
        });
    }

    /// Set one floating dimension, retaining the existing minimum-size floor.
    pub fn set_floating_size(
        &mut self,
        window_id: WindowId,
        size: i32,
        horizontal: bool,
        area: Rect,
    ) {
        self.change_floating_rect(window_id, area, |rect| {
            let dimension = if horizontal {
                &mut rect.width
            } else {
                &mut rect.height
            };
            *dimension = size.max(MIN_COLUMN_WIDTH.min(*dimension));
        });
    }

    /// Center a floating window in the work area.
    pub fn center_floating(&mut self, window_id: WindowId, area: Rect) {
        self.change_floating_rect(window_id, area, |rect| {
            rect.x = area
                .x
                .saturating_add(area.width.saturating_sub(rect.width) / 2);
            rect.y = area
                .y
                .saturating_add(area.height.saturating_sub(rect.height) / 2);
        });
    }

    pub fn floating_windows(&self) -> &[FloatingWindow] {
        &self.floating_windows
    }

    /// Sum active columns' effective widths and inter-column gaps, treating negative gaps as zero.
    pub fn total_width(&self) -> i32 {
        let active_columns: Vec<&Column> = self
            .columns
            .iter()
            .filter(|c| self.is_column_active(c))
            .collect();

        if active_columns.is_empty() {
            return 0;
        }

        let gap = self.gap.max(0);

        // Outer gaps are viewport padding, not strip content.
        let column_widths: i32 = active_columns
            .iter()
            .map(|c| self.effective_column_width(c))
            .fold(0i32, |acc, w| acc.saturating_add(w));
        let gaps = gap.saturating_mul(active_columns.len().saturating_sub(1) as i32);

        column_widths.saturating_add(gaps)
    }

    pub fn scroll_offset(&self) -> f64 {
        self.scroll_offset
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

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

    /// Return both tiled and floating window IDs.
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
    pub fn set_outer_gaps(&mut self, left: i32, right: i32, top: i32, bottom: i32) {
        self.outer_gap_left = left.max(0);
        self.outer_gap_right = right.max(0);
        self.outer_gap_top = top.max(0);
        self.outer_gap_bottom = bottom.max(0);
    }

    pub fn default_column_width(&self) -> i32 {
        self.default_column_width
    }

    /// Set the default width for new columns.
    /// Value is clamped to >= MIN_COLUMN_WIDTH (100px).
    pub fn set_default_column_width(&mut self, width: i32) {
        self.default_column_width = width.max(MIN_COLUMN_WIDTH);
    }

    /// Set the pixels reserved at the top of Tabbed columns for the tab strip overlay.
    /// Value is clamped to >= 0. Vertical columns ignore this value.
    pub fn set_tab_strip_reserve_px(&mut self, px: i32) {
        self.tab_strip_reserve_px = px.max(0);
    }

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

    /// A column is active when at least one window is not minimized.
    pub(crate) fn is_column_active(&self, column: &Column) -> bool {
        column
            .windows()
            .iter()
            .any(|w| !self.minimized_windows.contains(w))
    }
}
