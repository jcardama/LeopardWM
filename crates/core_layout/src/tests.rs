use super::*;

// Test-only helper methods for direct state manipulation
#[cfg(test)]
impl Workspace {
    /// Set focus state directly without validation (test helper).
    pub fn test_set_focus_unchecked(&mut self, column: usize, win: usize) {
        self.focused_column = column;
        self.focused_window_in_column = win;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_empty_workspace() {
        let ws = Workspace::new();
        assert!(ws.is_empty());
        assert_eq!(ws.column_count(), 0);
        assert_eq!(ws.total_width(), 0);
    }

    #[test]
    fn test_insert_window() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();

        assert!(!ws.is_empty());
        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_insert_multiple_windows() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(600)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        assert_eq!(ws.column_count(), 3);
        // Last inserted window should be focused
        assert_eq!(ws.focused_column_index(), 2);
        assert_eq!(ws.focused_window(), Some(3));

        // Total strip width: 400 + gap + 600 + gap + 400
        // = 400 + 10 + 600 + 10 + 400 = 1420
        // (outer gaps are viewport padding, not strip content)
        assert_eq!(ws.total_width(), 1420);
    }

    #[test]
    fn test_focus_navigation() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        assert_eq!(ws.focused_column_index(), 2); // Last inserted

        ws.focus_left();
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(2));

        ws.focus_left();
        assert_eq!(ws.focused_column_index(), 0);

        // Should not go below 0
        ws.focus_left();
        assert_eq!(ws.focused_column_index(), 0);

        ws.focus_right();
        ws.focus_right();
        assert_eq!(ws.focused_column_index(), 2);

        // Should not go beyond last column
        ws.focus_right();
        assert_eq!(ws.focused_column_index(), 2);
    }

    #[test]
    fn test_remove_window() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        assert_eq!(ws.column_count(), 3);

        ws.remove_window(2).unwrap();
        assert_eq!(ws.column_count(), 2);

        // Windows 1 and 3 should remain
        assert!(ws.columns().iter().any(|c| c.contains(1)));
        assert!(ws.columns().iter().any(|c| c.contains(3)));
    }

    #[test]
    fn test_compute_placements_visibility() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap(); // x: 10-410
        ws.insert_window(2, Some(400)).unwrap(); // x: 420-820
        ws.insert_window(3, Some(400)).unwrap(); // x: 830-1230

        ws.set_scroll_offset(0.0);

        // Viewport of 500px wide starting at (0, 0)
        let viewport = Rect::new(0, 0, 500, 600);
        let placements = ws.compute_placements(viewport);

        assert_eq!(placements.len(), 3);

        // First column should be visible
        assert_eq!(placements[0].visibility, Visibility::Visible);
        assert_eq!(placements[0].window_id, 1);

        // Second column partially visible
        assert_eq!(placements[1].visibility, Visibility::Visible);

        // Third column off-screen right
        assert_eq!(placements[2].visibility, Visibility::OffScreenRight);
    }

    #[test]
    fn test_ensure_focused_visible_center() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_centering_mode(CenteringMode::Center);

        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        ws.test_set_focus_unchecked(0, 0);
        ws.set_scroll_offset(500.0); // Start scrolled right

        ws.ensure_focused_visible(500);

        // Should center column 0 in the viewport
        // Column 0 is at x=10, width=400, center=210
        // Viewport width=500, center=250
        // scroll_offset = 210 - 250 = -40, clamped to 0
        assert_eq!(ws.scroll_offset(), 0.0);
    }

    #[test]
    fn test_stacked_windows() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();

        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].len(), 3);

        let viewport = Rect::new(0, 0, 500, 600);
        let placements = ws.compute_placements(viewport);

        assert_eq!(placements.len(), 3);
        // All three windows should be in the same column
        assert!(placements.iter().all(|p| p.column_index == 0));
    }

    #[test]
    fn test_resize_column() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();

        assert_eq!(ws.columns()[0].width(), 400);

        ws.resize_focused_column(100);
        assert_eq!(ws.columns()[0].width(), 500);

        ws.resize_focused_column(-200);
        assert_eq!(ws.columns()[0].width(), 300);

        // Should not go below minimum (100)
        ws.resize_focused_column(-500);
        assert_eq!(ws.columns()[0].width(), 100);
    }

    #[test]
    fn test_move_column() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        ws.test_set_focus_unchecked(1, 0);
        ws.move_column_left();

        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.columns()[0].get(0), Some(2));
        assert_eq!(ws.columns()[1].get(0), Some(1));

        ws.move_column_right();
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.columns()[0].get(0), Some(1));
        assert_eq!(ws.columns()[1].get(0), Some(2));
    }

    #[test]
    fn test_reorder_column_forward() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        // Focus on col 0 (window 1)
        ws.test_set_focus_unchecked(0, 0);

        // Move col 0 to col 2: [1,2,3] → [2,3,1]
        ws.reorder_column(0, 2);
        assert_eq!(ws.columns()[0].get(0), Some(2));
        assert_eq!(ws.columns()[1].get(0), Some(3));
        assert_eq!(ws.columns()[2].get(0), Some(1));
        // Focus should track the moved column
        assert_eq!(ws.focused_column_index(), 2);
    }

    #[test]
    fn test_reorder_column_backward() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        ws.test_set_focus_unchecked(2, 0);

        // Move col 2 to col 0: [1,2,3] → [3,1,2]
        ws.reorder_column(2, 0);
        assert_eq!(ws.columns()[0].get(0), Some(3));
        assert_eq!(ws.columns()[1].get(0), Some(1));
        assert_eq!(ws.columns()[2].get(0), Some(2));
        assert_eq!(ws.focused_column_index(), 0);
    }

    #[test]
    fn test_reorder_column_noop() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.reorder_column(0, 0);
        assert_eq!(ws.columns()[0].get(0), Some(1));
        assert_eq!(ws.columns()[1].get(0), Some(2));
    }

    #[test]
    fn test_reorder_column_out_of_bounds() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        // Should not panic
        ws.reorder_column(0, 5);
        ws.reorder_column(5, 0);
        assert_eq!(ws.columns()[0].get(0), Some(1));
    }

    #[test]
    fn test_reorder_non_focused_column_adjusts_focus() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        // Focus col 1 (window 2)
        ws.test_set_focus_unchecked(1, 0);

        // Move col 0 to col 2: [1,2,3] → [2,3,1]
        // Focused col was 1 which is in (from=0, to=2] range → shifts left to 0
        ws.reorder_column(0, 2);
        assert_eq!(ws.focused_column_index(), 0);
        // The focused column still has window 2
        assert_eq!(ws.columns()[ws.focused_column_index()].get(0), Some(2));
    }

    #[test]
    fn test_remove_column() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        ws.test_set_focus_unchecked(2, 0);

        let col = ws.remove_column(1);
        assert!(col.is_some());
        assert_eq!(col.unwrap().get(0), Some(2));
        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].get(0), Some(1));
        assert_eq!(ws.columns()[1].get(0), Some(3));
        // Focus was 2, removed index 1 → focus stays at 1 (now points to window 3's column)
        assert_eq!(ws.focused_column_index(), 1);
    }

    #[test]
    fn test_remove_column_out_of_bounds() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        assert!(ws.remove_column(5).is_none());
        assert_eq!(ws.column_count(), 1);
    }

    #[test]
    fn test_insert_column_at() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.test_set_focus_unchecked(0, 0);

        // Insert a new column at index 0
        let col = Column::new(99, 300);
        ws.insert_column_at(col, 0);
        assert_eq!(ws.column_count(), 3);
        assert_eq!(ws.columns()[0].get(0), Some(99));
        assert_eq!(ws.columns()[1].get(0), Some(1));
        assert_eq!(ws.columns()[2].get(0), Some(2));
        // Focus was 0, insertion at 0 → shifts to 1
        assert_eq!(ws.focused_column_index(), 1);
    }

    #[test]
    fn test_insert_column_at_end() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.test_set_focus_unchecked(0, 0);

        let col = Column::new(99, 300);
        ws.insert_column_at(col, 100); // clamped to len
        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[1].get(0), Some(99));
        assert_eq!(ws.focused_column_index(), 0); // unchanged
    }

    #[test]
    fn test_scroll_by() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        let viewport_width = 500;
        // Visible width = viewport - outer_left - outer_right = 500 - 10 - 10 = 480
        let vis_w = viewport_width - 10 - 10;

        ws.scroll_by(100.0, viewport_width);
        assert_eq!(ws.scroll_offset(), 100.0);

        ws.scroll_by(2000.0, viewport_width);
        // Should clamp to max scroll (total_width - visible_width)
        let max_scroll = (ws.total_width() - vis_w).max(0) as f64;
        assert_eq!(ws.scroll_offset(), max_scroll);

        ws.scroll_by(-5000.0, viewport_width);
        assert_eq!(ws.scroll_offset(), 0.0);
    }

    #[test]
    fn test_rect_intersects() {
        let r1 = Rect::new(0, 0, 100, 100);
        let r2 = Rect::new(50, 50, 100, 100);
        let r3 = Rect::new(200, 200, 50, 50);

        assert!(r1.intersects(&r2));
        assert!(r2.intersects(&r1));
        assert!(!r1.intersects(&r3));
        assert!(!r3.intersects(&r1));
    }

    // ====== Tests added from code review (Cycle 1) ======

    #[test]
    fn test_remove_last_window_empties_workspace() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();

        ws.remove_window(1).unwrap();

        assert!(ws.is_empty());
        assert_eq!(ws.column_count(), 0);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window_index_in_column(), 0);
        assert_eq!(ws.scroll_offset(), 0.0);
    }

    #[test]
    fn test_ensure_focused_visible_just_in_view() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_centering_mode(CenteringMode::JustInView);

        ws.insert_window(1, Some(200)).unwrap(); // x: 10-210
        ws.insert_window(2, Some(200)).unwrap(); // x: 220-420
        ws.insert_window(3, Some(200)).unwrap(); // x: 430-630

        ws.test_set_focus_unchecked(0, 0);
        ws.set_scroll_offset(0.0);

        // Column 0 is already in view - should NOT scroll
        ws.ensure_focused_visible(500);
        assert_eq!(ws.scroll_offset(), 0.0);

        // Focus column 2, which is partially out of view
        ws.test_set_focus_unchecked(2, 0);
        ws.ensure_focused_visible(500);
        // Should scroll just enough to bring column 2 into view
        assert!(ws.scroll_offset() > 0.0);
    }

    #[test]
    fn test_compute_placements_tight_viewport() {
        let mut ws = Workspace::with_gaps(10, 50); // Large outer_gap
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();

        // Viewport smaller than outer_gaps * 2
        let viewport = Rect::new(0, 0, 500, 80); // Only 80px tall
        let placements = ws.compute_placements(viewport);

        // All heights should be >= 0
        for p in &placements {
            assert!(p.rect.height >= 0, "Height was negative: {}", p.rect.height);
        }
    }

    #[test]
    fn test_insert_window_clamps_width() {
        let mut ws = Workspace::new();

        // Try to insert with zero width
        ws.insert_window(1, Some(0)).unwrap();
        assert_eq!(ws.columns()[0].width(), 100); // Clamped to minimum

        // Try to insert with negative width
        ws.insert_window(2, Some(-50)).unwrap();
        assert_eq!(ws.columns()[1].width(), 100); // Clamped to minimum
    }

    #[test]
    fn test_scroll_offset_rounding() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        // Set fractional scroll offset
        ws.set_scroll_offset(100.7);

        let viewport = Rect::new(0, 0, 500, 600);
        let placements = ws.compute_placements(viewport);

        // Verify placements use rounded value (101, not truncated 100)
        // The first column at x=10 should be at screen x = 10 - 101 + 0 = -91
        assert_eq!(placements[0].rect.x, -91);
    }

    // ====== Tests added from code review (Cycle 2) ======

    #[test]
    fn test_column_empty_constructor() {
        let col = Column::empty(50);
        assert_eq!(col.width(), 100); // Clamped to MIN_COLUMN_WIDTH
        assert!(col.is_empty());
        assert_eq!(col.len(), 0);
    }

    #[test]
    fn test_rect_right_and_bottom() {
        let r = Rect::new(10, 20, 100, 50);
        assert_eq!(r.right(), 110);
        assert_eq!(r.bottom(), 70);

        // Edge case: negative coordinates
        let r2 = Rect::new(-50, -30, 100, 80);
        assert_eq!(r2.right(), 50);
        assert_eq!(r2.bottom(), 50);
    }

    #[test]
    fn test_focus_operations_on_empty_workspace() {
        let mut ws = Workspace::new();

        // All focus operations should safely do nothing
        ws.focus_left();
        ws.focus_right();
        ws.focus_up();
        ws.focus_down();

        assert!(ws.focused_window().is_none());
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window_index_in_column(), 0);
    }

    #[test]
    fn test_remove_nonexistent_window() {
        let mut ws = Workspace::new();
        let result = ws.remove_window(999);
        assert!(matches!(result, Err(LayoutError::WindowNotFound(999))));
    }

    #[test]
    fn test_remove_window_adjusts_focus_correctly() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        // Focus on column 2 (window 3)
        ws.test_set_focus_unchecked(2, 0);

        // Remove from column 0
        ws.remove_window(1).unwrap();

        // Focus should adjust: was 2, column 0 removed, now should be 1
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(3));
    }

    #[test]
    fn test_remove_window_clamps_focus_index_after_column_removal() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap(); // col 0
        ws.insert_window(2, Some(400)).unwrap(); // col 1
        ws.insert_window(3, Some(400)).unwrap(); // col 2
        ws.insert_window_in_column(4, 2).unwrap();
        ws.insert_window_in_column(5, 2).unwrap(); // col 2: [3, 4, 5]

        // Simulate stale focus index before removing a column.
        ws.test_set_focus_unchecked(1, 99);
        ws.remove_window(2).unwrap(); // remove col 1, focus shifts to old col 2

        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window_index_in_column(), 2);
        assert_eq!(ws.focused_window(), Some(5));
    }

    #[test]
    fn test_duplicate_window_rejected() {
        let mut ws = Workspace::new();
        ws.insert_window(42, Some(400)).unwrap();

        // Try to insert same window as new column
        let result = ws.insert_window(42, Some(400));
        assert!(matches!(result, Err(LayoutError::DuplicateWindow(42))));

        // Try to insert same window into existing column
        let result = ws.insert_window_in_column(42, 0);
        assert!(matches!(result, Err(LayoutError::DuplicateWindow(42))));

        // Workspace should still have only one column with one window
        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].len(), 1);
    }

    #[test]
    fn test_rect_clamps_negative_dimensions() {
        let r = Rect::new(10, 20, -100, -50);
        assert_eq!(r.width, 0);
        assert_eq!(r.height, 0);
        assert_eq!(r.x, 10);
        assert_eq!(r.y, 20);
    }

    #[test]
    fn test_total_width_saturates() {
        let mut ws = Workspace::new();

        // Insert many columns with large widths to test saturation
        for i in 0..1000 {
            ws.insert_window(i, Some(i32::MAX / 100)).unwrap();
        }

        // Should saturate to i32::MAX instead of overflowing/panicking
        let width = ws.total_width();
        assert!(width > 0); // Should not wrap to negative
        assert_eq!(width, i32::MAX); // Should saturate at max
    }

    // ====== Tests added from code review (Cycle 4) ======

    #[test]
    fn test_focus_window_by_id() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        // Focus is on column 2 (window 3) after inserts
        assert_eq!(ws.focused_window(), Some(3));

        // Focus window 1 by ID
        ws.focus_window(1).unwrap();
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(1));

        // Focus window 2 by ID
        ws.focus_window(2).unwrap();
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(2));

        // Try to focus nonexistent window
        let result = ws.focus_window(999);
        assert!(matches!(result, Err(LayoutError::WindowNotFound(999))));
    }

    #[test]
    fn test_set_focus_validates() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window_in_column(3, 1).unwrap(); // Stack window 3 in column 1

        // Valid focus
        ws.set_focus(1, 1).unwrap();
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window_index_in_column(), 1);
        assert_eq!(ws.focused_window(), Some(3));

        // Invalid column index
        let result = ws.set_focus(5, 0);
        assert!(matches!(result, Err(LayoutError::ColumnOutOfBounds(5, 1))));

        // Invalid window index in column
        let result = ws.set_focus(0, 10);
        assert!(matches!(
            result,
            Err(LayoutError::WindowIndexOutOfBounds(10, 0, 0))
        ));
    }

    #[test]
    fn test_scroll_by_special_floats() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        let viewport_width = 500;

        // Scroll to a known position
        ws.scroll_by(50.0, viewport_width);
        assert_eq!(ws.scroll_offset(), 50.0);

        // NaN should be treated as zero (no change)
        ws.scroll_by(f64::NAN, viewport_width);
        assert_eq!(ws.scroll_offset(), 50.0);

        // Infinity should be treated as zero (no change)
        ws.scroll_by(f64::INFINITY, viewport_width);
        assert_eq!(ws.scroll_offset(), 50.0);

        // Negative infinity should be treated as zero (no change)
        ws.scroll_by(f64::NEG_INFINITY, viewport_width);
        assert_eq!(ws.scroll_offset(), 50.0);
    }

    #[test]
    fn test_column_width_getter() {
        let col = Column::new(1, 500);
        assert_eq!(col.width(), 500);

        let col2 = Column::new(2, 50); // Below minimum
        assert_eq!(col2.width(), 100); // Clamped
    }

    #[test]
    fn test_column_contains() {
        let mut col = Column::new(1, 400);
        col.add_window(2);
        col.add_window(3);

        assert!(col.contains(1));
        assert!(col.contains(2));
        assert!(col.contains(3));
        assert!(!col.contains(999));

        // Test get() method
        assert_eq!(col.get(0), Some(1));
        assert_eq!(col.get(1), Some(2));
        assert_eq!(col.get(2), Some(3));
        assert_eq!(col.get(10), None);

        // Test windows() slice
        assert_eq!(col.windows(), &[1, 2, 3]);
    }

    // ====== Tests added from code review (Cycle 5) ======

    #[test]
    fn test_remove_window_before_focus_in_stacked_column() {
        // Bug test: removing a window BEFORE the focused window in a stacked column
        // should keep focus on the same window (index should decrement)
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap(); // Column 0
        ws.insert_window_in_column(2, 0).unwrap(); // Stack: [1, 2]
        ws.insert_window_in_column(3, 0).unwrap(); // Stack: [1, 2, 3]

        // Focus on window 2 (index 1)
        ws.test_set_focus_unchecked(0, 1);
        assert_eq!(ws.focused_window(), Some(2));

        // Remove window 1 (index 0, before focused)
        ws.remove_window(1).unwrap();

        // Focus should still be on window 2, but index should now be 0
        assert_eq!(ws.focused_window(), Some(2));
        assert_eq!(ws.focused_window_index_in_column(), 0);
    }

    #[test]
    fn test_remove_focused_window_in_stacked_column() {
        // Removing the focused window should move focus to next window (or previous if at end)
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap(); // Stack: [1, 2, 3]

        // Focus on window 2 (index 1, middle)
        ws.test_set_focus_unchecked(0, 1);
        assert_eq!(ws.focused_window(), Some(2));

        // Remove window 2 (the focused window)
        ws.remove_window(2).unwrap();

        // Stack is now [1, 3], focus index 1 should point to window 3 (next)
        assert_eq!(ws.focused_window(), Some(3));
        assert_eq!(ws.focused_window_index_in_column(), 1);
    }

    #[test]
    fn test_remove_last_focused_window_in_stacked_column() {
        // Removing the last focused window should move focus to previous
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap(); // Stack: [1, 2, 3]

        // Focus on window 3 (index 2, last)
        ws.test_set_focus_unchecked(0, 2);
        assert_eq!(ws.focused_window(), Some(3));

        // Remove window 3 (the focused window, at end)
        ws.remove_window(3).unwrap();

        // Stack is now [1, 2], focus should move to index 1 (window 2)
        assert_eq!(ws.focused_window(), Some(2));
        assert_eq!(ws.focused_window_index_in_column(), 1);
    }

    #[test]
    fn test_compute_placements_wide_column() {
        // Column wider than viewport
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(1000)).unwrap(); // Column wider than viewport

        let viewport = Rect::new(0, 0, 500, 600); // Viewport only 500px wide
        let placements = ws.compute_placements(viewport);

        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].visibility, Visibility::Visible);
        assert_eq!(placements[0].rect.width, 1000); // Full column width preserved
    }

    #[test]
    fn test_column_empty_type() {
        // Tests the Column::empty() constructor and its properties.
        // Note: In practice, empty columns are automatically removed from workspaces
        // when the last window is removed, so empty columns don't occur in normal use.
        // Column::empty() exists for construction purposes (e.g., building columns
        // before adding windows).
        let empty_col = Column::empty(300);
        assert!(empty_col.is_empty());
        assert_eq!(empty_col.width(), 300);
        assert_eq!(empty_col.len(), 0);
        assert_eq!(empty_col.windows(), &[]);

        // Verify workspace doesn't produce placements for non-existent windows
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        let viewport = Rect::new(0, 0, 2000, 600);
        let placements = ws.compute_placements(viewport);
        assert_eq!(placements.len(), 2); // Only 2 windows, no extras
    }

    #[test]
    fn test_negative_gaps_clamped() {
        // Negative gaps should be clamped to 0
        let ws = Workspace::with_gaps(-100, -50);
        assert_eq!(ws.gap(), 0);
        assert_eq!(ws.outer_gaps(), (0, 0, 0, 0));
    }

    #[test]
    fn test_gap_setters_clamp() {
        let mut ws = Workspace::new();

        // Test gap setter
        ws.set_gap(20);
        assert_eq!(ws.gap(), 20);
        ws.set_gap(-50);
        assert_eq!(ws.gap(), 0); // Clamped

        // Test outer_gap setter
        ws.set_outer_gaps(15, 15, 15, 15);
        assert_eq!(ws.outer_gaps(), (15, 15, 15, 15));
        ws.set_outer_gaps(-100, -100, -100, -100);
        assert_eq!(ws.outer_gaps(), (0, 0, 0, 0)); // Clamped

        // Test default_column_width setter
        ws.set_default_column_width(500);
        assert_eq!(ws.default_column_width(), 500);
        ws.set_default_column_width(50); // Below MIN_COLUMN_WIDTH
        assert_eq!(ws.default_column_width(), 100); // Clamped to minimum

        // Test centering_mode getter/setter
        assert_eq!(ws.centering_mode(), CenteringMode::Center); // Default
        ws.set_centering_mode(CenteringMode::JustInView);
        assert_eq!(ws.centering_mode(), CenteringMode::JustInView);
    }

    #[test]
    fn test_compute_placements_spacing_integrity() {
        // Verify stacked window heights + gaps sum correctly
        let mut ws = Workspace::with_gaps(10, 20);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap(); // Stack: [1, 2, 3]

        let viewport = Rect::new(0, 0, 500, 600);
        let placements = ws.compute_placements(viewport);

        // usable_height = 600 - 20*2 = 560
        // 3 windows with 2 gaps = 560 - 10*2 = 540 for windows
        // Each window ~180px, but last takes remainder

        let total_height: i32 = placements.iter().map(|p| p.rect.height).sum();
        let (_, _, outer_top, outer_bottom) = ws.outer_gaps();
        let expected_usable = viewport.height - outer_top - outer_bottom;
        let expected_gaps = ws.gap() * (placements.len() as i32 - 1);

        // Total heights + gaps should equal usable height
        assert_eq!(total_height + expected_gaps, expected_usable);
    }

    #[test]
    fn test_column_remove_returns_index() {
        let mut col = Column::new(1, 400);
        col.add_window(2);
        col.add_window(3);
        // Windows: [1, 2, 3]

        // Remove middle window
        let removed = col.remove_window(2);
        assert_eq!(removed, Some(1)); // Index 1

        // Remove first window
        let removed = col.remove_window(1);
        assert_eq!(removed, Some(0)); // Index 0

        // Remove nonexistent
        let removed = col.remove_window(999);
        assert_eq!(removed, None);
    }

    // ====== Tests added from code review (Cycle 7) ======

    #[test]
    fn test_compute_placements_zero_viewport_width() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        // Zero width viewport - edge case
        let viewport = Rect::new(0, 0, 0, 600);
        let placements = ws.compute_placements(viewport);

        // Should produce placements without panicking
        assert_eq!(placements.len(), 2);
        // All columns should be off-screen right (viewport has no width)
        for p in &placements {
            assert_eq!(p.visibility, Visibility::OffScreenRight);
        }
    }

    #[test]
    fn test_compute_placements_zero_viewport_height() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();

        // Zero height viewport - edge case
        let viewport = Rect::new(0, 0, 500, 0);
        let placements = ws.compute_placements(viewport);

        // Should produce placements without panicking
        assert_eq!(placements.len(), 2);
        // All heights should be >= 0 (clamped)
        for p in &placements {
            assert!(p.rect.height >= 0, "Height was negative: {}", p.rect.height);
        }
    }

    #[test]
    fn test_focus_navigation_clamps_to_shorter_column() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap(); // Column 0: [1]
        ws.insert_window(2, Some(400)).unwrap(); // Column 1: [2]
        ws.insert_window_in_column(3, 0).unwrap(); // Column 0: [1, 3]
        ws.insert_window_in_column(4, 0).unwrap(); // Column 0: [1, 3, 4]

        // Focus on window 4 (column 0, index 2)
        ws.test_set_focus_unchecked(0, 2);
        assert_eq!(ws.focused_window(), Some(4));

        // Move right to column 1 which only has 1 window
        ws.focus_right();

        // Focus should clamp to index 0 (the only window in column 1)
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window_index_in_column(), 0);
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_resize_then_ensure_focused_visible() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_centering_mode(CenteringMode::JustInView);
        ws.insert_window(1, Some(200)).unwrap();
        ws.insert_window(2, Some(200)).unwrap();
        ws.insert_window(3, Some(200)).unwrap();

        // Focus column 2, resize it significantly
        ws.test_set_focus_unchecked(2, 0);
        ws.resize_focused_column(500); // Now 700px wide

        // Ensure focused visible should adjust scroll
        ws.ensure_focused_visible(500);

        // Should have scrolled to bring the widened column into view
        assert!(ws.scroll_offset() > 0.0);
    }

    #[test]
    fn test_move_column_then_resize() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(300)).unwrap();
        ws.insert_window(3, Some(500)).unwrap();

        // Focus column 1 (window 2, 300px)
        ws.test_set_focus_unchecked(1, 0);
        assert_eq!(ws.columns()[1].width(), 300);

        // Move column left
        ws.move_column_left();
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.columns()[0].width(), 300); // Column with window 2

        // Resize the moved column
        ws.resize_focused_column(100);
        assert_eq!(ws.columns()[0].width(), 400);
    }

    #[test]
    fn test_remove_reinsert_same_window_id() {
        let mut ws = Workspace::new();
        ws.insert_window(42, Some(400)).unwrap();
        ws.insert_window(100, Some(300)).unwrap();

        // Remove window 42
        ws.remove_window(42).unwrap();
        assert!(!ws.contains_window(42));

        // Re-insert same ID should work now
        ws.insert_window(42, Some(500)).unwrap();
        assert!(ws.contains_window(42));
        assert_eq!(ws.focused_window(), Some(42));
    }

    #[test]
    fn test_find_window_location() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap(); // Column 0
        ws.insert_window(2, Some(400)).unwrap(); // Column 1
        ws.insert_window_in_column(3, 0).unwrap(); // Column 0, index 1
        ws.insert_window_in_column(4, 1).unwrap(); // Column 1, index 1

        assert_eq!(ws.find_window_location(1), Some((0, 0)));
        assert_eq!(ws.find_window_location(2), Some((1, 0)));
        assert_eq!(ws.find_window_location(3), Some((0, 1)));
        assert_eq!(ws.find_window_location(4), Some((1, 1)));
        assert_eq!(ws.find_window_location(999), None);
    }

    #[test]
    fn test_window_count() {
        let mut ws = Workspace::new();
        assert_eq!(ws.window_count(), 0);

        ws.insert_window(1, Some(400)).unwrap();
        assert_eq!(ws.window_count(), 1);

        ws.insert_window(2, Some(400)).unwrap();
        assert_eq!(ws.window_count(), 2);

        ws.insert_window_in_column(3, 0).unwrap();
        ws.insert_window_in_column(4, 0).unwrap();
        assert_eq!(ws.window_count(), 4);

        ws.remove_window(2).unwrap();
        assert_eq!(ws.window_count(), 3);
    }

    #[test]
    fn test_column_safe_access() {
        let mut ws = Workspace::new();
        assert!(ws.column(0).is_none());

        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(500)).unwrap();

        assert!(ws.column(0).is_some());
        assert_eq!(ws.column(0).unwrap().width(), 400);
        assert!(ws.column(1).is_some());
        assert_eq!(ws.column(1).unwrap().width(), 500);
        assert!(ws.column(2).is_none());
        assert!(ws.column(100).is_none());
    }

    #[test]
    fn test_single_column_move_operations() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();

        // Move operations on single column should do nothing
        ws.move_column_left();
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(1));

        ws.move_column_right();
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_resize_on_empty_workspace() {
        let mut ws = Workspace::new();

        // Resize on empty workspace should do nothing without panic
        ws.resize_focused_column(100);
        ws.resize_focused_column(-100);
        ws.resize_focused_column(i32::MAX);
        ws.resize_focused_column(i32::MIN);

        assert!(ws.is_empty());
    }

    #[test]
    fn test_invariants_after_complex_sequence() {
        let mut ws = Workspace::with_gaps(10, 10);

        // Insert several windows
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(300)).unwrap();
        ws.insert_window(3, Some(500)).unwrap();
        ws.insert_window_in_column(4, 1).unwrap();
        ws.insert_window_in_column(5, 1).unwrap();

        // Complex sequence of operations
        ws.focus_left();
        ws.focus_down();
        ws.move_column_right();
        ws.resize_focused_column(100);
        ws.focus_right();
        ws.focus_up();
        ws.remove_window(4).unwrap();
        ws.focus_left();
        ws.move_column_left();

        // Verify invariants still hold
        assert!(ws.focused_column < ws.columns.len());
        assert!(ws.focused_window_in_column < ws.columns[ws.focused_column].len());

        // No duplicate windows
        let mut all_windows: Vec<WindowId> = ws
            .columns
            .iter()
            .flat_map(|c| c.windows().iter().copied())
            .collect();
        all_windows.sort();
        let len_before = all_windows.len();
        all_windows.dedup();
        assert_eq!(all_windows.len(), len_before, "Duplicate windows found");
    }

    #[test]
    fn test_column_partial_eq() {
        let col1 = Column::new(1, 400);
        let col2 = Column::new(1, 400);
        let col3 = Column::new(2, 400);
        let col4 = Column::new(1, 500);

        assert_eq!(col1, col2);
        assert_ne!(col1, col3); // Different window
        assert_ne!(col1, col4); // Different width
    }

    // ========================================================================
    // Animation Tests
    // ========================================================================

    #[test]
    fn test_easing_linear() {
        assert!((Easing::Linear.apply(0.0) - 0.0).abs() < f64::EPSILON);
        assert!((Easing::Linear.apply(0.25) - 0.25).abs() < f64::EPSILON);
        assert!((Easing::Linear.apply(0.5) - 0.5).abs() < f64::EPSILON);
        assert!((Easing::Linear.apply(0.75) - 0.75).abs() < f64::EPSILON);
        assert!((Easing::Linear.apply(1.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_easing_ease_out() {
        // EaseOut starts fast, ends slow (cubic)
        let ease_out = Easing::EaseOut;

        // At t=0, should be 0
        assert!((ease_out.apply(0.0) - 0.0).abs() < f64::EPSILON);
        // At t=1, should be 1
        assert!((ease_out.apply(1.0) - 1.0).abs() < f64::EPSILON);

        // EaseOut should be ahead of linear in the middle
        assert!(ease_out.apply(0.5) > 0.5);

        // Verify cubic formula: 1 - (1 - t)^3
        let t: f64 = 0.5;
        let expected = 1.0 - (1.0 - t).powi(3);
        assert!((ease_out.apply(t) - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn test_easing_ease_in() {
        // EaseIn starts slow, ends fast (cubic)
        let ease_in = Easing::EaseIn;

        // At t=0, should be 0
        assert!((ease_in.apply(0.0) - 0.0).abs() < f64::EPSILON);
        // At t=1, should be 1
        assert!((ease_in.apply(1.0) - 1.0).abs() < f64::EPSILON);

        // EaseIn should be behind linear in the middle
        assert!(ease_in.apply(0.5) < 0.5);

        // Verify cubic formula: t^3
        let t: f64 = 0.5;
        let expected = t.powi(3);
        assert!((ease_in.apply(t) - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn test_easing_ease_in_out() {
        let ease_in_out = Easing::EaseInOut;

        // At t=0, should be 0
        assert!((ease_in_out.apply(0.0) - 0.0).abs() < f64::EPSILON);
        // At t=1, should be 1
        assert!((ease_in_out.apply(1.0) - 1.0).abs() < f64::EPSILON);
        // At t=0.5, should be 0.5 (inflection point)
        assert!((ease_in_out.apply(0.5) - 0.5).abs() < f64::EPSILON);

        // First half should be behind linear
        assert!(ease_in_out.apply(0.25) < 0.25);
        // Second half should be ahead of linear
        assert!(ease_in_out.apply(0.75) > 0.75);
    }

    #[test]
    fn test_easing_clamps_input() {
        // Values outside [0, 1] should be clamped
        assert!((Easing::Linear.apply(-0.5) - 0.0).abs() < f64::EPSILON);
        assert!((Easing::Linear.apply(1.5) - 1.0).abs() < f64::EPSILON);
        assert!((Easing::EaseOut.apply(-1.0) - 0.0).abs() < f64::EPSILON);
        assert!((Easing::EaseOut.apply(2.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_easing_default_is_ease_out() {
        assert_eq!(Easing::default(), Easing::EaseOut);
    }

    #[test]
    fn test_scroll_animation_new() {
        let anim = ScrollAnimation::new(0.0, 100.0, 200, Easing::Linear);

        assert!((anim.start_offset - 0.0).abs() < f64::EPSILON);
        assert!((anim.target_offset - 100.0).abs() < f64::EPSILON);
        assert_eq!(anim.duration_ms, 200);
        assert_eq!(anim.elapsed_ms, 0);
        assert_eq!(anim.easing, Easing::Linear);
    }

    #[test]
    fn test_scroll_animation_with_defaults() {
        let anim = ScrollAnimation::with_defaults(50.0, 150.0);

        assert!((anim.start_offset - 50.0).abs() < f64::EPSILON);
        assert!((anim.target_offset - 150.0).abs() < f64::EPSILON);
        assert_eq!(anim.duration_ms, DEFAULT_ANIMATION_DURATION_MS);
        assert_eq!(anim.easing, Easing::default());
    }

    #[test]
    fn test_scroll_animation_progress() {
        let mut anim = ScrollAnimation::new(0.0, 100.0, 100, Easing::Linear);

        assert!((anim.progress() - 0.0).abs() < f64::EPSILON);

        anim.elapsed_ms = 50;
        assert!((anim.progress() - 0.5).abs() < f64::EPSILON);

        anim.elapsed_ms = 100;
        assert!((anim.progress() - 1.0).abs() < f64::EPSILON);

        // Over time should clamp to 1.0
        anim.elapsed_ms = 150;
        assert!((anim.progress() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_scroll_animation_progress_zero_duration() {
        let anim = ScrollAnimation::new(0.0, 100.0, 0, Easing::Linear);
        // Zero duration should return 1.0 progress immediately
        assert!((anim.progress() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_scroll_animation_current_offset_linear() {
        let mut anim = ScrollAnimation::new(0.0, 100.0, 100, Easing::Linear);

        // At start
        assert!((anim.current_offset() - 0.0).abs() < f64::EPSILON);

        // At midpoint
        anim.elapsed_ms = 50;
        assert!((anim.current_offset() - 50.0).abs() < f64::EPSILON);

        // At end
        anim.elapsed_ms = 100;
        assert!((anim.current_offset() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_scroll_animation_current_offset_eased() {
        let mut anim = ScrollAnimation::new(0.0, 100.0, 100, Easing::EaseOut);

        // At midpoint with ease out, should be ahead of 50
        anim.elapsed_ms = 50;
        assert!(anim.current_offset() > 50.0);
    }

    #[test]
    fn test_scroll_animation_negative_direction() {
        let mut anim = ScrollAnimation::new(100.0, 0.0, 100, Easing::Linear);

        // At start
        assert!((anim.current_offset() - 100.0).abs() < f64::EPSILON);

        // At midpoint - should be halfway back to 0
        anim.elapsed_ms = 50;
        assert!((anim.current_offset() - 50.0).abs() < f64::EPSILON);

        // At end
        anim.elapsed_ms = 100;
        assert!((anim.current_offset() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_scroll_animation_is_complete() {
        let mut anim = ScrollAnimation::new(0.0, 100.0, 100, Easing::Linear);

        assert!(!anim.is_complete());

        anim.elapsed_ms = 50;
        assert!(!anim.is_complete());

        anim.elapsed_ms = 100;
        assert!(anim.is_complete());

        anim.elapsed_ms = 150;
        assert!(anim.is_complete());
    }

    #[test]
    fn test_scroll_animation_tick() {
        let mut anim = ScrollAnimation::new(0.0, 100.0, 100, Easing::Linear);

        // Tick returns true while running
        assert!(anim.tick(25));
        assert_eq!(anim.elapsed_ms, 25);

        assert!(anim.tick(25));
        assert_eq!(anim.elapsed_ms, 50);

        assert!(anim.tick(25));
        assert_eq!(anim.elapsed_ms, 75);

        // Final tick completes
        assert!(!anim.tick(25));
        assert_eq!(anim.elapsed_ms, 100);

        // Further ticks still return false
        assert!(!anim.tick(50));
        assert_eq!(anim.elapsed_ms, 150);
    }

    #[test]
    fn test_scroll_animation_tick_saturating() {
        let mut anim = ScrollAnimation::new(0.0, 100.0, 100, Easing::Linear);

        // Large tick value should not overflow
        anim.elapsed_ms = u64::MAX - 10;
        anim.tick(100);
        assert_eq!(anim.elapsed_ms, u64::MAX); // Saturates at MAX
    }

    #[test]
    fn test_scroll_animation_target() {
        let anim = ScrollAnimation::new(0.0, 456.78, 100, Easing::Linear);
        assert!((anim.target() - 456.78).abs() < f64::EPSILON);
    }

    // ========================================================================
    // Workspace Animation Tests
    // ========================================================================

    #[test]
    fn test_workspace_is_animating() {
        let mut ws = Workspace::with_gaps(10, 10);
        // Add enough windows to have scrollable content
        for i in 1..=5 {
            ws.insert_window(i, Some(400)).unwrap();
        }
        // Total: 10 + (5*400) + (4*10) + 10 = 2060

        assert!(!ws.is_animating());

        // Viewport 500 means max_scroll = 2060 - 500 = 1560
        ws.start_scroll_animation(100.0, 500, None, None);
        assert!(ws.is_animating());

        // Complete the animation
        ws.tick_animation(300);
        assert!(!ws.is_animating());
    }

    #[test]
    fn test_workspace_effective_scroll_offset() {
        let mut ws = Workspace::with_gaps(10, 10);
        // Add enough windows to have scrollable content
        for i in 1..=5 {
            ws.insert_window(i, Some(400)).unwrap();
        }
        // Total: 10 + (5*400) + (4*10) + 10 = 2060

        // Initially no animation
        assert!((ws.effective_scroll_offset() - 0.0).abs() < 1.0);

        // Start animation to 200 with viewport 500 (max_scroll = 1560)
        ws.start_scroll_animation(200.0, 500, Some(100), Some(Easing::Linear));
        assert!(ws.is_animating());

        // At start, should be near 0
        assert!(ws.effective_scroll_offset() < 10.0);

        // Tick halfway
        ws.tick_animation(50);
        // Should be around 100 (halfway)
        assert!(ws.effective_scroll_offset() > 80.0 && ws.effective_scroll_offset() < 120.0);
    }

    #[test]
    fn test_workspace_start_scroll_animation_clamps_target() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();

        // Total width: 10 + 400 + 10 = 420
        // Max scroll with 1000 viewport = max(420 - 1000, 0) = 0

        ws.start_scroll_animation(500.0, 1000, None, None);

        // Target should be clamped to 0 (can't scroll past content)
        assert!(!ws.is_animating()); // Already at target (both clamped to 0)
    }

    #[test]
    fn test_workspace_tick_animation() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        ws.start_scroll_animation(200.0, 500, Some(100), Some(Easing::Linear));

        // Should be animating
        assert!(ws.tick_animation(30));
        assert!(ws.is_animating());

        // Tick to completion
        assert!(!ws.tick_animation(100));
        assert!(!ws.is_animating());

        // After animation, scroll_offset should be at target
        assert!((ws.effective_scroll_offset() - 200.0).abs() < 1.0);
    }

    #[test]
    fn test_workspace_stop_animation() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        ws.start_scroll_animation(200.0, 500, Some(100), Some(Easing::Linear));
        ws.tick_animation(50);

        // Stop should snap to target
        ws.stop_animation();
        assert!(!ws.is_animating());
        assert!((ws.effective_scroll_offset() - 200.0).abs() < 1.0);
    }

    #[test]
    fn test_workspace_cancel_animation() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        ws.start_scroll_animation(200.0, 500, Some(100), Some(Easing::Linear));
        ws.tick_animation(50);

        let current = ws.effective_scroll_offset();
        // Should be around 100 (halfway)

        // Cancel should stay at current position
        ws.cancel_animation();
        assert!(!ws.is_animating());
        assert!((ws.effective_scroll_offset() - current).abs() < 1.0);
    }

    #[test]
    fn test_workspace_animation_no_effect_when_at_target() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();

        // Already at offset 0, trying to animate to 0 shouldn't start animation
        ws.start_scroll_animation(0.0, 1000, None, None);
        assert!(!ws.is_animating());
    }

    #[test]
    fn test_workspace_animation_interrupt() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        // Start animation to 200
        ws.start_scroll_animation(200.0, 500, Some(100), Some(Easing::Linear));
        ws.tick_animation(50);

        // Interrupt with new animation to 300
        ws.start_scroll_animation(300.0, 500, Some(100), Some(Easing::Linear));

        // New animation should start from current position (~100)
        assert!(ws.is_animating());

        // Complete new animation
        ws.tick_animation(150);
        assert!((ws.effective_scroll_offset() - 300.0).abs() < 1.0);
    }

    #[test]
    fn test_compute_placements_animated() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        let viewport = Rect::new(0, 0, 500, 600);

        // Without animation
        let placements1 = ws.compute_placements_animated(viewport);
        assert_eq!(placements1.len(), 2);

        // Start animation that shifts viewport
        ws.start_scroll_animation(200.0, 500, Some(100), Some(Easing::Linear));
        ws.tick_animation(100); // Complete

        let placements2 = ws.compute_placements_animated(viewport);
        assert_eq!(placements2.len(), 2);

        // Window positions should be shifted left (viewport scrolled right)
        assert!(placements2[0].rect.x < placements1[0].rect.x);
    }

    #[test]
    fn test_compute_placements_animated_matches_remainder_height_split() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();

        // usable_height = 101 - 20 = 81, gaps = 20, base per window = 20, remainder = 1
        let viewport = Rect::new(0, 0, 500, 101);
        let non_animated = ws.compute_placements(viewport);
        let animated = ws.compute_placements_animated(viewport);

        let non_animated_heights: Vec<(WindowId, i32)> = non_animated
            .iter()
            .map(|p| (p.window_id, p.rect.height))
            .collect();
        let animated_heights: Vec<(WindowId, i32)> = animated
            .iter()
            .map(|p| (p.window_id, p.rect.height))
            .collect();

        assert_eq!(animated_heights, non_animated_heights);
        assert_eq!(animated_heights, vec![(1, 20), (2, 20), (3, 21)]);
    }

    #[test]
    fn test_ensure_focused_visible_animated_center_mode() {
        let mut ws = Workspace::with_gaps(10, 10);
        // Add enough windows to require scrolling
        for i in 1..=5 {
            ws.insert_window(i, Some(400)).unwrap();
        }
        // Total: 10 + (5*400) + (4*10) + 10 = 2060

        // Focus is at column 4 (last inserted), scroll to make it visible first
        ws.ensure_focused_visible(500);

        // Now focus first column which is off-screen
        ws.focus_left();
        ws.focus_left();
        ws.focus_left();
        ws.focus_left();

        // This should trigger an animation because column 0 is now off-screen
        ws.ensure_focused_visible_animated(500);

        // Should start an animation to scroll back to column 0
        assert!(ws.is_animating());
    }

    // ========================================================================
    // Floating Window Tests
    // ========================================================================

    #[test]
    fn test_add_floating_window() {
        let mut ws = Workspace::new();
        let rect = Rect::new(100, 100, 400, 300);

        ws.add_floating(1, rect).unwrap();

        assert!(ws.contains_window(1));
        assert!(ws.is_floating(1));
        assert_eq!(ws.floating_count(), 1);
        assert_eq!(ws.column_count(), 0); // Not in columns
    }

    #[test]
    fn test_floating_window_in_placements() {
        let mut ws = Workspace::new();
        let rect = Rect::new(100, 100, 400, 300);
        let viewport = Rect::new(0, 0, 1920, 1080);

        ws.add_floating(1, rect).unwrap();

        let placements = ws.compute_placements(viewport);
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].window_id, 1);
        assert_eq!(placements[0].rect, rect);
        assert_eq!(placements[0].visibility, Visibility::Visible);
        assert_eq!(placements[0].column_index, usize::MAX); // Floating sentinel
    }

    #[test]
    fn test_remove_floating_window() {
        let mut ws = Workspace::new();
        let rect = Rect::new(100, 100, 400, 300);

        ws.add_floating(1, rect).unwrap();
        assert!(ws.contains_window(1));

        let removed = ws.remove_floating(1);
        assert!(removed);
        assert!(!ws.contains_window(1));
        assert_eq!(ws.floating_count(), 0);
    }

    #[test]
    fn test_remove_nonexistent_floating_window() {
        let mut ws = Workspace::new();
        let removed = ws.remove_floating(999);
        assert!(!removed);
    }

    #[test]
    fn test_update_floating_window() {
        let mut ws = Workspace::new();
        let rect1 = Rect::new(100, 100, 400, 300);
        let rect2 = Rect::new(200, 200, 600, 400);

        ws.add_floating(1, rect1).unwrap();

        let updated = ws.update_floating(1, rect2);
        assert!(updated);

        let placements = ws.compute_placements(Rect::new(0, 0, 1920, 1080));
        assert_eq!(placements[0].rect, rect2);
    }

    #[test]
    fn test_floating_and_tiled_windows_together() {
        let mut ws = Workspace::with_gaps(10, 10);
        let floating_rect = Rect::new(500, 500, 300, 200);
        let viewport = Rect::new(0, 0, 1920, 1080);

        // Add tiled window
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        // Add floating window
        ws.add_floating(3, floating_rect).unwrap();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.floating_count(), 1);
        assert!(ws.contains_window(1));
        assert!(ws.contains_window(2));
        assert!(ws.contains_window(3));
        assert!(!ws.is_floating(1));
        assert!(!ws.is_floating(2));
        assert!(ws.is_floating(3));

        let placements = ws.compute_placements(viewport);
        assert_eq!(placements.len(), 3);

        // Floating window should be last in placements
        assert_eq!(placements[2].window_id, 3);
        assert_eq!(placements[2].rect, floating_rect);
    }

    #[test]
    fn test_duplicate_floating_window_rejected() {
        let mut ws = Workspace::new();
        let rect = Rect::new(100, 100, 400, 300);

        ws.add_floating(1, rect).unwrap();

        let result = ws.add_floating(1, rect);
        assert!(matches!(result, Err(LayoutError::DuplicateWindow(1))));
    }

    #[test]
    fn test_floating_window_duplicate_with_tiled() {
        let mut ws = Workspace::new();

        // Add tiled window
        ws.insert_window(1, Some(400)).unwrap();

        // Try to add same ID as floating - should fail
        let result = ws.add_floating(1, Rect::new(100, 100, 400, 300));
        assert!(matches!(result, Err(LayoutError::DuplicateWindow(1))));
    }

    // ====================================================================
    // Fullscreen Tests
    // ====================================================================

    #[test]
    fn test_fullscreen_toggle() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();

        assert!(!ws.is_fullscreen());

        // Enter fullscreen
        let entered = ws.toggle_fullscreen();
        assert!(entered);
        assert!(ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), Some(1));

        // Exit fullscreen
        let entered = ws.toggle_fullscreen();
        assert!(!entered);
        assert!(!ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), None);
    }

    #[test]
    fn test_fullscreen_exit_clears_min_width() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();

        // Enter fullscreen
        ws.toggle_fullscreen();
        assert!(ws.is_fullscreen());

        // Simulate a width violation recorded while the window was at
        // viewport size (e.g. video player resisting resize during animation).
        ws.set_window_min_width(1, 1920);

        // Exit fullscreen — min-width should be cleared
        let entered = ws.toggle_fullscreen();
        assert!(!entered);

        // Column width should remain at the original value, not inflated
        assert_eq!(ws.columns()[0].width(), 400);

        // apply_min_width_constraints should not widen anything
        assert!(!ws.apply_min_width_constraints());
    }

    #[test]
    fn test_toggle_fullscreen_targets_visible_window_when_focus_is_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();

        // Focus remains on window 1 after stacking; minimize it so the visible
        // fallback in the same column is window 2.
        ws.mark_minimized(1);
        assert_eq!(ws.focused_window(), Some(1));
        assert_eq!(ws.focused_visible_window(), Some(2));

        // Simulate stale fullscreen state that points at a minimized window.
        ws.fullscreen_window = Some(1);
        let entered = ws.toggle_fullscreen();
        assert!(entered);
        assert_eq!(ws.fullscreen_window_id(), Some(2));
    }

    #[test]
    fn test_fullscreen_empty_workspace() {
        let mut ws = Workspace::new();
        let entered = ws.toggle_fullscreen();
        assert!(!entered);
        assert!(!ws.is_fullscreen());
    }

    #[test]
    fn test_fullscreen_placements() {
        let mut ws = Workspace::with_gaps(10, 10);
        let viewport = Rect::new(0, 0, 1920, 1080);

        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        // Focus window 2 and make it fullscreen
        ws.focus_left();
        assert_eq!(ws.focused_window(), Some(2));
        ws.toggle_fullscreen();

        let placements = ws.compute_placements(viewport);
        assert_eq!(placements.len(), 3);

        // Window 2 should cover the full viewport
        let fs = placements.iter().find(|p| p.window_id == 2).unwrap();
        assert_eq!(fs.rect, viewport);
        assert_eq!(fs.visibility, Visibility::Visible);

        // Others should be off-screen
        let w1 = placements.iter().find(|p| p.window_id == 1).unwrap();
        assert_eq!(w1.visibility, Visibility::OffScreenLeft);

        let w3 = placements.iter().find(|p| p.window_id == 3).unwrap();
        assert_eq!(w3.visibility, Visibility::OffScreenLeft);
    }

    #[test]
    fn test_fullscreen_animated_placements() {
        let mut ws = Workspace::with_gaps(10, 10);
        let viewport = Rect::new(0, 0, 1920, 1080);

        ws.insert_window(1, Some(400)).unwrap();
        ws.toggle_fullscreen();

        let placements = ws.compute_placements_animated(viewport);
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].rect, viewport);
        assert_eq!(placements[0].visibility, Visibility::Visible);
    }

    #[test]
    fn test_fullscreen_placements_fallback_when_fullscreen_window_is_minimized() {
        let mut ws = Workspace::with_gaps(10, 10);
        let viewport = Rect::new(0, 0, 1920, 1080);

        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        ws.mark_minimized(1);
        ws.fullscreen_window = Some(1); // stale/minimized fullscreen target

        let placements = ws.compute_placements(viewport);
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].window_id, 2);
        assert_eq!(placements[0].visibility, Visibility::Visible);
    }

    #[test]
    fn test_remove_fullscreen_tiled_window_clears_fullscreen() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        let _ = ws.focus_window(1);
        ws.toggle_fullscreen();
        assert_eq!(ws.fullscreen_window_id(), Some(1));

        ws.remove_window(1).unwrap();
        assert!(!ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), None);

        let viewport = Rect::new(0, 0, 1920, 1080);
        let placements = ws.compute_placements(viewport);
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].window_id, 2);
        assert_eq!(placements[0].visibility, Visibility::Visible);
    }

    #[test]
    fn test_remove_fullscreen_floating_window_clears_fullscreen() {
        let mut ws = Workspace::new();
        ws.add_floating(10, Rect::new(100, 100, 500, 400)).unwrap();
        ws.fullscreen_window = Some(10);
        assert!(ws.is_fullscreen());

        assert!(ws.remove_floating(10));
        assert!(!ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), None);
    }

    #[test]
    fn test_clear_fullscreen_if_window_clears_for_floating_target() {
        let mut ws = Workspace::new();
        ws.add_floating(10, Rect::new(100, 100, 500, 400)).unwrap();
        ws.fullscreen_window = Some(10);
        assert!(ws.is_fullscreen());

        assert!(ws.clear_fullscreen_if_window(10));
        assert!(!ws.is_fullscreen());
    }

    // ====================================================================
    // Toggle Floating Tests
    // ====================================================================

    #[test]
    fn test_toggle_floating_tiled_to_float() {
        let mut ws = Workspace::new();
        let viewport = Rect::new(0, 0, 1920, 1080);

        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        // Focus window 2 and toggle to floating
        let wid = ws.toggle_floating(viewport);
        assert_eq!(wid, Some(2));
        assert!(ws.is_floating(2));
        assert_eq!(ws.column_count(), 1); // Only window 1 tiled
        assert_eq!(ws.floating_count(), 1);
    }

    #[test]
    fn test_toggle_floating_clears_fullscreen_for_focused_window() {
        let mut ws = Workspace::new();
        let viewport = Rect::new(0, 0, 1920, 1080);

        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.toggle_fullscreen();
        assert_eq!(ws.fullscreen_window_id(), Some(2));

        let wid = ws.toggle_floating(viewport);
        assert_eq!(wid, Some(2));
        assert!(ws.is_floating(2));
        assert_eq!(ws.fullscreen_window_id(), None);
    }

    #[test]
    fn test_toggle_floating_back() {
        let mut ws = Workspace::new();
        let viewport = Rect::new(0, 0, 1920, 1080);

        ws.insert_window(1, Some(400)).unwrap();

        // Toggle to floating
        let wid = ws.toggle_floating(viewport);
        assert_eq!(wid, Some(1));
        assert!(ws.is_floating(1));
        assert_eq!(ws.column_count(), 0);

        // Use unfloat to bring it back
        let ok = ws.unfloat_window(1);
        assert!(ok);
        assert!(!ws.is_floating(1));
        assert_eq!(ws.column_count(), 1);
    }

    #[test]
    fn test_toggle_floating_empty_workspace() {
        let mut ws = Workspace::new();
        let viewport = Rect::new(0, 0, 1920, 1080);
        let wid = ws.toggle_floating(viewport);
        assert_eq!(wid, None);
    }

    // ====================================================================
    // Column Width Preset Tests
    // ====================================================================

    #[test]
    fn test_set_column_width_fraction() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();

        // base = 1920 - 10 - 10 + 10 = 1910
        // width = fraction * base - gap
        ws.set_focused_column_width_fraction(0.5, 1920);
        assert_eq!(ws.columns()[0].width(), 945); // 0.5 * 1910 - 10

        ws.set_focused_column_width_fraction(0.333, 1920);
        assert_eq!(ws.columns()[0].width(), 626); // round(0.333 * 1910 - 10)
    }

    #[test]
    fn test_set_column_width_fraction_clamp() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();

        // Fraction is clamped to [0.1, 1.0]
        // Default outer_gap = 10, so usable = 1920 - 20 = 1900
        ws.set_focused_column_width_fraction(2.0, 1920);
        let w = ws.columns()[0].width();
        // fraction clamped to 1.0: (1920 - 20 outer) * 1.0 = 1900
        assert_eq!(w, 1900);
    }

    #[test]
    fn test_equalize_widths() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(300)).unwrap();
        ws.insert_window(2, Some(600)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        // 3 columns, viewport 1920
        // total_gaps = 10*2 + 10*2 = 40
        // per_column = (1920 - 40) / 3 = 626
        ws.equalize_column_widths(1920);

        assert_eq!(ws.columns()[0].width(), 626);
        assert_eq!(ws.columns()[1].width(), 626);
        assert_eq!(ws.columns()[2].width(), 626);
    }

    #[test]
    fn test_equalize_widths_empty() {
        let mut ws = Workspace::new();
        ws.equalize_column_widths(1920); // Should not panic
    }

    #[test]
    fn test_unfloat_nonexistent() {
        let mut ws = Workspace::new();
        assert!(!ws.unfloat_window(999));
    }

    // ====================================================================
    // Minimize Tests
    // ====================================================================

    #[test]
    fn test_mark_minimized_managed_window() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        assert!(ws.mark_minimized(1));
        assert!(ws.is_minimized(1));
        assert_eq!(ws.minimized_count(), 1);
    }

    #[test]
    fn test_mark_minimized_unknown_window() {
        let mut ws = Workspace::new();
        assert!(!ws.mark_minimized(999));
        assert!(!ws.is_minimized(999));
        assert_eq!(ws.minimized_count(), 0);
    }

    #[test]
    fn test_mark_minimized_floating_window() {
        let mut ws = Workspace::new();
        ws.add_floating(1, Rect::new(0, 0, 100, 100)).unwrap();
        // Floating windows can be marked minimized
        assert!(ws.mark_minimized(1));
        assert!(ws.is_minimized(1));
    }

    #[test]
    fn test_mark_minimized_idempotent() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        assert!(ws.mark_minimized(1));
        // Second call returns false (already in set)
        assert!(!ws.mark_minimized(1));
        assert_eq!(ws.minimized_count(), 1);
    }

    #[test]
    fn test_mark_restored() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.mark_minimized(1);
        assert!(ws.mark_restored(1));
        assert!(!ws.is_minimized(1));
        assert_eq!(ws.minimized_count(), 0);
    }

    #[test]
    fn test_mark_restored_not_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        // Not minimized, restore returns false
        assert!(!ws.mark_restored(1));
    }

    #[test]
    fn test_placements_skip_minimized() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        let viewport = Rect::new(0, 0, 1920, 1080);
        let placements_before = ws.compute_placements(viewport);
        assert_eq!(placements_before.len(), 2);

        ws.mark_minimized(1);
        let placements_after = ws.compute_placements(viewport);
        // Only window 2 gets a placement
        assert_eq!(placements_after.len(), 1);
        assert_eq!(placements_after[0].window_id, 2);
    }

    #[test]
    fn test_placements_animated_skip_minimized() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        let viewport = Rect::new(0, 0, 1920, 1080);
        ws.mark_minimized(1);
        let placements = ws.compute_placements_animated(viewport);
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].window_id, 2);
    }

    #[test]
    fn test_minimize_height_redistribution() {
        // Two windows stacked in one column; minimizing one gives the other full height
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();

        let viewport = Rect::new(0, 0, 400, 1000);
        let before = ws.compute_placements(viewport);
        assert_eq!(before.len(), 2);
        // Each window gets half: 500px
        assert_eq!(before[0].rect.height, 500);
        assert_eq!(before[1].rect.height, 500);

        ws.mark_minimized(1);
        let after = ws.compute_placements(viewport);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].window_id, 2);
        // Window 2 now gets the full height
        assert_eq!(after[0].rect.height, 1000);
    }

    #[test]
    fn test_minimize_all_in_column() {
        // Minimizing all windows in a column produces no placements for that column
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();

        ws.mark_minimized(1);
        ws.mark_minimized(2);

        let viewport = Rect::new(0, 0, 400, 1000);
        let placements = ws.compute_placements(viewport);
        assert!(placements.is_empty());
    }

    #[test]
    fn test_remove_window_clears_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.mark_minimized(1);
        assert_eq!(ws.minimized_count(), 1);

        ws.remove_window(1).unwrap();
        assert_eq!(ws.minimized_count(), 0);
        assert!(!ws.is_minimized(1));
    }

    #[test]
    fn test_all_window_ids_includes_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.mark_minimized(1);

        let ids = ws.all_window_ids();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn test_contains_window_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.mark_minimized(1);
        // Still contained in workspace
        assert!(ws.contains_window(1));
    }

    #[test]
    fn test_minimized_window_count_unchanged() {
        // window_count counts all tiled windows, including minimized
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        assert_eq!(ws.window_count(), 2);

        ws.mark_minimized(1);
        // window_count stays the same (minimized windows still in columns)
        assert_eq!(ws.window_count(), 2);
    }

    // ====================================================================
    // Fullscreen + Minimize Interaction Tests
    // ====================================================================

    #[test]
    fn test_fullscreen_minimize_clears_fullscreen() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        // Focus window 1 and enter fullscreen
        let _ = ws.focus_window(1);
        ws.toggle_fullscreen();
        assert!(ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), Some(1));

        // Minimize the fullscreen window — should exit fullscreen
        ws.mark_minimized(1);
        assert!(!ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), None);

        // Other windows should now get normal placements
        let viewport = Rect::new(0, 0, 1920, 1080);
        let placements = ws.compute_placements(viewport);
        let w2 = placements.iter().find(|p| p.window_id == 2).unwrap();
        assert_eq!(w2.visibility, Visibility::Visible);
    }

    #[test]
    fn test_fullscreen_minimize_restore_cycle() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        // Enter fullscreen on window 1
        let _ = ws.focus_window(1);
        ws.toggle_fullscreen();
        assert!(ws.is_fullscreen());

        // Minimize fullscreen window
        ws.mark_minimized(1);
        assert!(!ws.is_fullscreen());

        // Restore window 1
        ws.mark_restored(1);
        // Fullscreen is NOT automatically re-entered (user must re-enter manually)
        assert!(!ws.is_fullscreen());
    }

    #[test]
    fn test_minimize_non_fullscreen_window_keeps_fullscreen() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        // Fullscreen window 1
        let _ = ws.focus_window(1);
        ws.toggle_fullscreen();
        assert!(ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), Some(1));

        // Minimize window 2 (not the fullscreen window)
        ws.mark_minimized(2);
        // Fullscreen should remain active
        assert!(ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), Some(1));
    }

    // ====================================================================
    // Focus Navigation Skips Minimized Windows
    // ====================================================================

    #[test]
    fn test_focus_left_skips_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        // Focus is on window 3 (rightmost)
        assert_eq!(ws.focused_window(), Some(3));

        // Minimize window 2 (middle column — all-minimized column)
        ws.mark_minimized(2);

        // Focus left should automatically skip the all-minimized column
        ws.focus_left();
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_focus_right_skips_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        // Focus window 1 (leftmost)
        let _ = ws.focus_window(1);
        assert_eq!(ws.focused_window(), Some(1));

        // Minimize window 2 (middle)
        ws.mark_minimized(2);

        // Focus right should skip the all-minimized column
        ws.focus_right();
        assert_eq!(ws.focused_window(), Some(3));
    }

    #[test]
    fn test_focus_down_skips_minimized_in_stack() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap(); // stack below 1
        ws.insert_window_in_column(3, 0).unwrap(); // stack below 2

        // Focus window 1 (top)
        let _ = ws.focus_window(1);
        assert_eq!(ws.focused_window(), Some(1));

        // Minimize window 2 (middle)
        ws.mark_minimized(2);

        // Focus down should skip minimized window 2 and land on window 3
        ws.focus_down();
        assert_eq!(ws.focused_window(), Some(3));
    }

    #[test]
    fn test_focus_up_skips_minimized_in_stack() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();

        // Focus window 3 (bottom)
        let _ = ws.focus_window(3);
        assert_eq!(ws.focused_window(), Some(3));

        // Minimize window 2 (middle)
        ws.mark_minimized(2);

        // Focus up should skip minimized window 2 and land on window 1
        ws.focus_up();
        assert_eq!(ws.focused_window(), Some(1));
    }

    // ====================================================================
    // Focus in Mixed Columns (some minimized, some visible)
    // ====================================================================

    #[test]
    fn test_focus_left_into_mixed_column_lands_on_visible() {
        // Column 0 has [win1(minimized), win2(visible)] stacked.
        // Column 1 has [win3(visible)].
        // Focus is on win3; focus_left should land on win2, not win1.
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap(); // col 0
        ws.insert_window_in_column(2, 0).unwrap(); // col 0, stacked
        ws.insert_window(3, Some(400)).unwrap(); // col 1

        // Minimize win1 (top of col 0)
        ws.mark_minimized(1);

        // Focus win3 in col 1
        let _ = ws.focus_window(3);
        assert_eq!(ws.focused_window(), Some(3));

        // Navigate left into mixed column
        ws.focus_left();
        // Should land on win2 (the visible window), not win1 (minimized)
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_focus_right_into_mixed_column_lands_on_visible() {
        // Column 0 has [win1(visible)].
        // Column 1 has [win2(minimized), win3(visible)] stacked.
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap(); // col 0
        ws.insert_window(2, Some(400)).unwrap(); // col 1
        ws.insert_window_in_column(3, 1).unwrap(); // col 1, stacked

        // Minimize win2 (top of col 1)
        ws.mark_minimized(2);

        // Focus win1
        let _ = ws.focus_window(1);
        assert_eq!(ws.focused_window(), Some(1));

        // Navigate right into mixed column
        ws.focus_right();
        // Should land on win3 (the visible window), not win2 (minimized)
        assert_eq!(ws.focused_window(), Some(3));
    }

    #[test]
    fn test_focus_left_readjusts_visible_window_when_column_does_not_change() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.mark_minimized(1);

        let _ = ws.focus_window(1);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(1));

        // Already at left boundary, so column won't change.
        ws.focus_left();
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_focus_right_readjusts_visible_window_when_column_does_not_change() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window_in_column(3, 1).unwrap();
        ws.mark_minimized(2);

        let _ = ws.focus_window(2);
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(2));

        // Already at right boundary, so column won't change.
        ws.focus_right();
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(3));
    }

    #[test]
    fn test_focused_visible_window_skips_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();

        // Focus win1, then minimize it
        let _ = ws.focus_window(1);
        ws.mark_minimized(1);

        // focused_window() still returns win1 (raw index)
        assert_eq!(ws.focused_window(), Some(1));

        // focused_visible_window() should return win2 (nearest visible)
        assert_eq!(ws.focused_visible_window(), Some(2));
    }

    #[test]
    fn test_focused_visible_window_all_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.mark_minimized(1);

        // No visible window exists
        assert_eq!(ws.focused_visible_window(), None);
    }

    // ====================================================================
    // Minimize / Restore Scroll Clamping
    // ====================================================================

    #[test]
    fn test_minimize_clamps_stale_scroll_just_in_view() {
        // Regression: in JustInView mode, minimizing a column could leave
        // scroll_offset beyond the new max_scroll if the focused column
        // was "in view" relative to the stale scroll position.
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_centering_mode(CenteringMode::JustInView);

        // 3 columns of 800px each. total_width = 2440, viewport = 1920
        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window(2, Some(800)).unwrap();
        ws.insert_window(3, Some(800)).unwrap();

        let vp_width = 1920;

        // Scroll to show column 2 (rightmost)
        let _ = ws.focus_window(3);
        ws.ensure_focused_visible(vp_width);
        // max_scroll = 2440 - 1920 = 520
        assert_eq!(ws.scroll_offset as i32, 520);

        // Minimize column 2 (focused). Focus moves to column 1.
        ws.mark_minimized(3);
        ws.focus_right(); // fails (nothing to right)
        ws.focus_left(); // goes to col 1
        assert_eq!(ws.focused_window(), Some(2));

        // Animated ensure should clamp scroll to new max_scroll (0)
        ws.ensure_focused_visible_animated(vp_width);

        // Complete the animation
        for _ in 0..100 {
            if !ws.tick_animation(16) {
                break;
            }
        }

        let scroll = ws.effective_scroll_offset();
        let max_scroll = (ws.total_width() - vp_width).max(0) as f64;
        assert!(
            scroll <= max_scroll + 0.5,
            "scroll {} should be <= max_scroll {} after minimize",
            scroll,
            max_scroll
        );
    }

    #[test]
    fn test_minimize_single_window_at_first_position() {
        // When all but one window are minimized, the remaining window
        // should be placed at the first position (outer_gap from left).
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_centering_mode(CenteringMode::JustInView);

        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window(2, Some(800)).unwrap();
        ws.insert_window(3, Some(800)).unwrap();

        let viewport = Rect::new(0, 0, 1920, 1080);
        let vp_width = viewport.width;

        // Scroll right to view column 2
        let _ = ws.focus_window(3);
        ws.ensure_focused_visible(vp_width);

        // Minimize columns 0 and 2, leaving only column 1 active
        ws.mark_minimized(1);
        ws.mark_minimized(3);
        let _ = ws.focus_window(2);

        ws.ensure_focused_visible_animated(vp_width);
        // Complete the animation
        for _ in 0..100 {
            if !ws.tick_animation(16) {
                break;
            }
        }

        let placements = ws.compute_placements(viewport);
        assert_eq!(placements.len(), 1);
        // Should be at outer_gap (10), not offset by stale scroll
        assert_eq!(placements[0].rect.x, 10);
    }

    #[test]
    fn test_restore_all_positions_correct() {
        // Restore 3 windows one by one and verify no overlapping positions.
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_centering_mode(CenteringMode::JustInView);

        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window(2, Some(800)).unwrap();
        ws.insert_window(3, Some(800)).unwrap();

        let viewport = Rect::new(0, 0, 1920, 1080);
        let vp_width = viewport.width;

        // Minimize all
        ws.mark_minimized(1);
        ws.mark_minimized(2);
        ws.mark_minimized(3);
        ws.ensure_focused_visible_animated(vp_width);
        for _ in 0..100 {
            if !ws.tick_animation(16) {
                break;
            }
        }

        // Restore window 1
        ws.mark_restored(1);
        let _ = ws.focus_window(1);
        ws.ensure_focused_visible_animated(vp_width);
        for _ in 0..100 {
            if !ws.tick_animation(16) {
                break;
            }
        }

        let p = ws.compute_placements(viewport);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].rect.x, 10); // first position

        // Restore window 2
        ws.mark_restored(2);
        let _ = ws.focus_window(2);
        ws.ensure_focused_visible_animated(vp_width);
        for _ in 0..100 {
            if !ws.tick_animation(16) {
                break;
            }
        }

        let p = ws.compute_placements(viewport);
        assert_eq!(p.len(), 2);
        // Windows should be at different x positions
        assert_ne!(p[0].rect.x, p[1].rect.x);

        // Restore window 3
        ws.mark_restored(3);
        let _ = ws.focus_window(3);
        ws.ensure_focused_visible_animated(vp_width);
        for _ in 0..100 {
            if !ws.tick_animation(16) {
                break;
            }
        }

        let p = ws.compute_placements_animated(viewport);
        assert_eq!(p.len(), 3);
        // All at distinct x positions
        let xs: Vec<i32> = p.iter().map(|pl| pl.rect.x).collect();
        assert_ne!(xs[0], xs[1], "window 1 and 2 overlap at x={}", xs[0]);
        assert_ne!(xs[1], xs[2], "window 2 and 3 overlap at x={}", xs[1]);
        assert_ne!(xs[0], xs[2], "window 1 and 3 overlap at x={}", xs[0]);
    }

    // =========================================================================
    // insert_window_no_focus (R30-C1: focus_new_windows=false)
    // =========================================================================

    #[test]
    fn test_insert_window_no_focus_preserves_focus() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        // Focus is on window 3 (most recently inserted)
        assert_eq!(ws.focused_window(), Some(3));

        // Insert a 4th window without changing focus
        ws.insert_window_no_focus(4, Some(400)).unwrap();
        // Focus should still be on window 3, not 4
        assert_eq!(ws.focused_window(), Some(3));
        // The new window should exist in the workspace
        assert!(ws.contains_window(4));
        assert_eq!(ws.window_count(), 4);
    }

    #[test]
    fn test_insert_window_no_focus_into_empty_workspace() {
        let mut ws = Workspace::new();
        // When workspace is empty, the first window must get focus
        ws.insert_window_no_focus(1, Some(400)).unwrap();
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_insert_window_no_focus_duplicate_error() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        let result = ws.insert_window_no_focus(1, Some(400));
        assert!(result.is_err());
    }

    // =========================================================================
    // Consume / Expel / Move window in column
    // =========================================================================

    #[test]
    fn test_move_window_left_basic() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap(); // col 0: [A]
        ws.insert_window(2, Some(400)).unwrap(); // col 1: [B], focused
        assert_eq!(ws.focused_column_index(), 1);

        ws.move_window_left();

        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1, 2]); // B joined A's column
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(2)); // focus followed
    }

    #[test]
    fn test_move_window_left_multi() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap(); // col 0: [A]
        ws.insert_window(2, Some(400)).unwrap(); // col 1: [B]
        ws.insert_window_in_column(3, 1).unwrap(); // col 1: [B, C]
        // Focus B (idx 0 in col 1)
        ws.test_set_focus_unchecked(1, 0);

        ws.move_window_left();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[1, 2]); // B joined A
        assert_eq!(ws.columns()[1].windows(), &[3]); // C remains
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_move_window_left_at_edge() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        assert_eq!(ws.focused_column_index(), 0);

        ws.move_window_left(); // no-op

        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1]);
    }

    #[test]
    fn test_move_window_right_basic() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap(); // col 0: [A]
        ws.insert_window(2, Some(400)).unwrap(); // col 1: [B]
        ws.focus_left(); // focus col 0
        assert_eq!(ws.focused_column_index(), 0);

        ws.move_window_right();

        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[2, 1]); // A joined B's column
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(1)); // focus followed
    }

    #[test]
    fn test_move_window_right_multi() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap(); // col 0: [A]
        ws.insert_window_in_column(2, 0).unwrap(); // col 0: [A, B]
        ws.insert_window(3, Some(400)).unwrap(); // col 1: [C]
        // Focus A (idx 0 in col 0)
        ws.test_set_focus_unchecked(0, 0);

        ws.move_window_right();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[2]); // B remains
        assert_eq!(ws.columns()[1].windows(), &[3, 1]); // A joined C
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_move_window_right_at_edge() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        assert_eq!(ws.focused_column_index(), 0);

        ws.move_window_right(); // no-op

        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1]);
    }

    #[test]
    fn test_move_window_right_single_window_column() {
        // Single window in source — column removed after move
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap(); // col 0: [A]
        ws.insert_window(2, Some(400)).unwrap(); // col 1: [B]
        ws.insert_window(3, Some(400)).unwrap(); // col 2: [C]
        ws.focus_left();
        ws.focus_left(); // focus col 0
        assert_eq!(ws.focused_column_index(), 0);

        ws.move_window_right();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[2, 1]); // B + A
        assert_eq!(ws.columns()[1].windows(), &[3]);
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_expel_to_left() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();
        // col 0: [A, B, C], focus B (idx 1)
        ws.test_set_focus_unchecked(0, 1);

        ws.expel_to_left();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[2]); // B expelled
        assert_eq!(ws.columns()[1].windows(), &[1, 3]); // A, C remain
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_expel_to_right() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();
        // col 0: [A, B, C], focus B (idx 1)
        ws.test_set_focus_unchecked(0, 1);

        ws.expel_to_right();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[1, 3]); // A, C remain
        assert_eq!(ws.columns()[1].windows(), &[2]); // B expelled
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_expel_single_window_noop() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();

        ws.expel_to_left(); // no-op
        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1]);

        ws.expel_to_right(); // no-op
        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1]);
    }

    #[test]
    fn test_move_window_up() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();
        // col 0: [A, B, C], focus C (idx 2)
        ws.test_set_focus_unchecked(0, 2);

        ws.move_window_up_in_column();

        assert_eq!(ws.columns()[0].windows(), &[1, 3, 2]); // C swapped up
        assert_eq!(ws.focused_window_index_in_column(), 1);
        assert_eq!(ws.focused_window(), Some(3));
    }

    #[test]
    fn test_move_window_down() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();
        // col 0: [A, B, C], focus A (idx 0)
        ws.test_set_focus_unchecked(0, 0);

        ws.move_window_down_in_column();

        assert_eq!(ws.columns()[0].windows(), &[2, 1, 3]); // A swapped down
        assert_eq!(ws.focused_window_index_in_column(), 1);
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_move_window_up_at_top_noop() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.test_set_focus_unchecked(0, 0);

        ws.move_window_up_in_column();

        assert_eq!(ws.columns()[0].windows(), &[1, 2]);
        assert_eq!(ws.focused_window_index_in_column(), 0);
    }

    #[test]
    fn test_move_window_down_at_bottom_noop() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.test_set_focus_unchecked(0, 1);

        ws.move_window_down_in_column();

        assert_eq!(ws.columns()[0].windows(), &[1, 2]);
        assert_eq!(ws.focused_window_index_in_column(), 1);
    }

    #[test]
    fn test_move_window_preserves_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap(); // focused, col 1
        ws.mark_minimized(2);
        assert_eq!(ws.focused_column_index(), 1);

        ws.move_window_left();

        // Window 2 should still be in minimized set
        assert!(ws.is_minimized(2));
        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1, 2]);
    }

    // ========================================================================
    // Height Weights Tests
    // ========================================================================

    #[test]
    fn test_column_height_weights_on_add() {
        let mut col = Column::new(1, 800);
        assert_eq!(col.height_weights(), &[1.0]);

        col.add_window(2);
        assert_eq!(col.height_weights().len(), 2);
        let sum: f64 = col.height_weights().iter().sum();
        assert!((sum - 1.0).abs() < 1e-9);
        assert!((col.height_weights()[0] - 0.5).abs() < 1e-9);
        assert!((col.height_weights()[1] - 0.5).abs() < 1e-9);

        col.add_window(3);
        assert_eq!(col.height_weights().len(), 3);
        let sum: f64 = col.height_weights().iter().sum();
        assert!((sum - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_column_height_weights_on_remove() {
        let mut col = Column::new(1, 800);
        col.add_window(2);
        col.add_window(3);

        col.remove_window(2);
        assert_eq!(col.height_weights().len(), 2);
        let sum: f64 = col.height_weights().iter().sum();
        assert!((sum - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_column_height_weights_swap() {
        let mut col = Column::new(1, 800);
        col.add_window(2);
        // Set unequal weights
        col.set_height_weight(0, 0.667);
        let w0 = col.height_weights()[0];
        let w1 = col.height_weights()[1];
        assert!((w0 - 0.667).abs() < 0.01);

        col.swap_windows(0, 1);
        // Weights should follow their windows
        assert!((col.height_weights()[0] - w1).abs() < 1e-9);
        assert!((col.height_weights()[1] - w0).abs() < 1e-9);
    }

    #[test]
    fn test_placements_with_height_weights() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();

        // Set window 1 to 2/3 weight, window 2 to 1/3
        ws.columns[0].set_height_weight(0, 0.667);

        let viewport = Rect::new(0, 0, 800, 900);
        let placements = ws.compute_placements(viewport);

        assert_eq!(placements.len(), 2);
        // Window 1 (2/3 of 900 = 600)
        assert!((placements[0].rect.height - 600).abs() <= 1);
        // Window 2 (1/3 of 900 = 300)
        assert!((placements[1].rect.height - 300).abs() <= 1);
        // Total should equal viewport height
        assert_eq!(
            placements[0].rect.height + placements[1].rect.height,
            viewport.height
        );
    }

    #[test]
    fn test_cycle_width_up() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(640)).unwrap(); // 640/1920 ≈ 0.333
        let presets = vec![0.333, 0.5, 0.667];

        ws.cycle_width_up(&presets, 1920);
        let expected = (1920.0_f64 * 0.5).round() as i32;
        assert_eq!(ws.columns()[0].width(), expected);
    }

    #[test]
    fn test_cycle_width_down() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(1280)).unwrap(); // 1280/1920 ≈ 0.667
        let presets = vec![0.333, 0.5, 0.667];

        ws.cycle_width_down(&presets, 1920);
        let expected = (1920.0_f64 * 0.5).round() as i32;
        assert_eq!(ws.columns()[0].width(), expected);
    }

    #[test]
    fn test_cycle_width_between_presets() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(768)).unwrap(); // 768/1920 = 0.4
        let presets = vec![0.333, 0.5, 0.667];

        ws.cycle_width_up(&presets, 1920);
        let expected = (1920.0_f64 * 0.5).round() as i32;
        assert_eq!(ws.columns()[0].width(), expected);
    }

    #[test]
    fn test_cycle_width_at_max_noop() {
        let mut ws = Workspace::with_gaps(0, 0);
        let w = (1920.0_f64 * 0.667).round() as i32;
        ws.insert_window(1, Some(w)).unwrap();
        let presets = vec![0.333, 0.5, 0.667];

        ws.cycle_width_up(&presets, 1920);
        assert_eq!(ws.columns()[0].width(), w); // No change
    }

    #[test]
    fn test_cycle_height_single_window_noop() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(800)).unwrap();
        let presets = vec![0.333, 0.5, 0.667];

        ws.cycle_height_up(&presets);
        // Single window — weight should stay at 1.0
        assert_eq!(ws.columns()[0].height_weights(), &[1.0]);
    }

    #[test]
    fn test_cycle_height_multi_window() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.set_focus(0, 0).unwrap();

        // Both windows start at 0.5 weight
        let presets = vec![0.333, 0.5, 0.667];

        ws.cycle_height_up(&presets);
        let w = ws.columns()[0].height_weights()[0];
        assert!((w - 0.667).abs() < 0.01);
    }

    #[test]
    fn test_equalize_heights() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.set_focus(0, 0).unwrap();

        let presets = vec![0.333, 0.5, 0.667];
        ws.cycle_height_up(&presets);

        ws.equalize_focused_column_heights();
        assert!((ws.columns()[0].height_weights()[0] - 0.5).abs() < 1e-9);
        assert!((ws.columns()[0].height_weights()[1] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_height_weights_backward_compat() {
        // Simulate a Column deserialized without height_weights (empty vec via #[serde(default)])
        let col = Column {
            width: 800,
            windows: vec![1, 2],
            height_weights: Vec::new(), // backward compat: empty
        };
        assert!(col.height_weights.is_empty());
        // Compute placements should fall back to equal distribution
        let mut ws = Workspace::with_gaps(0, 0);
        ws.columns.push(col);
        ws.focused_column = 0;
        ws.focused_window_in_column = 0;
        let viewport = Rect::new(0, 0, 800, 1000);
        let placements = ws.compute_placements(viewport);
        assert_eq!(placements.len(), 2);
        // Equal heights
        assert_eq!(placements[0].rect.height + placements[1].rect.height, 1000);
        assert!((placements[0].rect.height - placements[1].rect.height).abs() <= 1);
    }

    #[test]
    fn test_move_window_resets_weights() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window(2, Some(800)).unwrap();
        ws.insert_window_in_column(3, 1).unwrap();
        ws.set_focus(1, 0).unwrap();

        // Set unequal weights in column 1
        ws.columns[1].set_height_weight(0, 0.667);

        // Move window 2 to column 0
        ws.set_focus(1, 0).unwrap();
        ws.move_window_left();

        // Column 0 now has 2 windows — weights should be equalized
        let weights = ws.columns[0].height_weights();
        assert_eq!(weights.len(), 2);
        assert!((weights[0] - 0.5).abs() < 1e-9);
        assert!((weights[1] - 0.5).abs() < 1e-9);
    }
}
