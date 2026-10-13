use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Minimum width for columns in pixels.
pub(crate) const MIN_COLUMN_WIDTH: i32 = 100;

/// Default gap between columns in pixels.
pub const DEFAULT_GAP: i32 = 10;
/// Default gap between rows in pixels.
pub const DEFAULT_ROW_GAP: i32 = 8;
/// Default outer gaps at viewport edges in pixels.
pub const DEFAULT_OUTER_GAP: i32 = 10;
/// Default width for new columns in pixels.
pub const DEFAULT_COLUMN_WIDTH: i32 = 800;

pub(crate) fn default_gap_value() -> i32 {
    DEFAULT_GAP
}

pub(crate) fn default_outer_gap_value() -> i32 {
    DEFAULT_OUTER_GAP
}

pub(crate) fn default_row_gap_value() -> i32 {
    DEFAULT_ROW_GAP
}

pub(crate) fn default_column_width_value() -> i32 {
    DEFAULT_COLUMN_WIDTH
}

/// Unique identifier for a window.
/// On Windows, this will typically be the HWND cast to u64.
pub type WindowId = u64;

/// Errors that can occur during layout operations.
#[derive(Debug, Error)]
pub enum LayoutError {
    #[error("Column index {0} is out of bounds (max: {1})")]
    ColumnOutOfBounds(usize, usize),

    #[error("Row index {0} is out of bounds (max: {1})")]
    RowOutOfBounds(usize, usize),

    #[error("Window {0} not found in workspace")]
    WindowNotFound(WindowId),

    #[error("Window {0} already exists in workspace")]
    DuplicateWindow(WindowId),

    #[error("Window index {0} is out of bounds in column {1} (max: {2})")]
    WindowIndexOutOfBounds(usize, usize, usize),
}

/// A rectangle in screen coordinates (pixels).
///
/// Note: Fields are intentionally public for convenient read access.
/// Use `Rect::new()` to construct with dimension validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    /// Create a new rectangle.
    /// Width and height are clamped to >= 0 to prevent invalid dimensions.
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width: width.max(0),
            height: height.max(0),
        }
    }

    /// Check if this rectangle intersects with another.
    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }

    /// Get the right edge x-coordinate.
    pub fn right(&self) -> i32 {
        self.x + self.width
    }

    /// Get the bottom edge y-coordinate.
    pub fn bottom(&self) -> i32 {
        self.y + self.height
    }

    /// Whether this rectangle contains the given point.
    pub fn contains_point(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }
}

/// Visibility state for layout computation.
/// Determines whether a window should be rendered or cloaked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    /// Window is within the viewport and should be rendered.
    Visible,
    /// Window is off-screen to the left of the viewport.
    OffScreenLeft,
    /// Window is off-screen to the right of the viewport.
    OffScreenRight,
}

/// Computed placement for a window.
/// Contains the target rectangle and visibility state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowPlacement {
    /// The window identifier.
    pub window_id: WindowId,
    /// The target rectangle in screen coordinates.
    pub rect: Rect,
    /// Whether the window is visible or off-screen.
    pub visibility: Visibility,
    /// The column index this window belongs to.
    pub column_index: usize,
    /// The row index this window belongs to (0-based).
    #[serde(default)]
    pub row_index: usize,
}

impl WindowPlacement {
    /// Create a new window placement.
    pub fn new(
        window_id: WindowId,
        rect: Rect,
        visibility: Visibility,
        column_index: usize,
        row_index: usize,
    ) -> Self {
        Self {
            window_id,
            rect,
            visibility,
            column_index,
            row_index,
        }
    }

    /// Helper for tiled window placement.
    pub fn tiled(
        window_id: WindowId,
        rect: Rect,
        visibility: Visibility,
        column_index: usize,
        row_index: usize,
    ) -> Self {
        Self::new(window_id, rect, visibility, column_index, row_index)
    }

    /// Helper for floating window placement (uses usize::MAX sentinel).
    pub fn floating(window_id: WindowId, rect: Rect, visibility: Visibility) -> Self {
        Self::new(window_id, rect, visibility, usize::MAX, usize::MAX)
    }
}

impl Default for WindowPlacement {
    fn default() -> Self {
        Self {
            window_id: 0,
            rect: Rect::new(0, 0, 0, 0),
            visibility: Visibility::Visible,
            column_index: 0,
            row_index: 0,
        }
    }
}
