use serde::{Deserialize, Serialize};

use crate::animation::ScrollAnimation;
use crate::column::Column;
use crate::types::WindowId;
use crate::workspace::MaximizedColumnState;

/// An independent horizontal scrolling strip of columns in a workspace.
///
/// In a multi-row workspace, each row is a separate strip with its own
/// columns, focus indices, and horizontal scroll offset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRow {
    /// Columns in this row, ordered left to right.
    pub(crate) columns: Vec<Column>,
    /// Index of the currently focused column in this row.
    pub(crate) focused_column: usize,
    /// Index of the focused window within the focused column.
    pub(crate) focused_window_in_column: usize,
    /// Current scroll offset (x position of viewport's left edge on this strip).
    pub(crate) scroll_offset: f64,
    /// Active scroll animation for this row, if any.
    #[serde(skip)]
    pub(crate) active_animation: Option<ScrollAnimation>,
    /// State for maximized column toggle within this row.
    #[serde(skip)]
    pub(crate) maximized_column: Option<MaximizedColumnState>,
}

impl Default for WorkspaceRow {
    fn default() -> Self {
        Self {
            columns: Vec::new(),
            focused_column: 0,
            focused_window_in_column: 0,
            scroll_offset: 0.0,
            active_animation: None,
            maximized_column: None,
        }
    }
}

impl WorkspaceRow {
    /// Create a new empty row.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if the row has no columns.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Get the number of columns in this row.
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Get a slice of all columns in this row.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Get a mutable slice of all columns in this row.
    pub fn columns_mut(&mut self) -> &mut [Column] {
        &mut self.columns
    }

    /// Get a column by index.
    pub fn column(&self, index: usize) -> Option<&Column> {
        self.columns.get(index)
    }

    /// Get a mutable reference to a column by index.
    pub fn column_mut(&mut self, index: usize) -> Option<&mut Column> {
        self.columns.get_mut(index)
    }

    /// Index of the currently focused column in this row.
    pub fn focused_column(&self) -> usize {
        self.focused_column
    }

    /// Index of the focused window within the focused column.
    pub fn focused_window_in_column(&self) -> usize {
        self.focused_window_in_column
    }

    /// Current scroll offset for this row.
    pub fn scroll_offset(&self) -> f64 {
        self.scroll_offset
    }

    /// Set the scroll offset for this row.
    pub fn set_scroll_offset(&mut self, offset: f64) {
        self.scroll_offset = offset;
    }

    /// Get the focused window in this row, if any.
    pub fn focused_window(&self) -> Option<WindowId> {
        self.columns
            .get(self.focused_column)
            .and_then(|col| col.windows().get(self.focused_window_in_column).copied())
    }

    /// Check if this row is currently animating scroll.
    pub fn is_animating(&self) -> bool {
        self.active_animation.is_some()
    }
}
