use super::*;

#[cfg(test)]
impl Workspace {
    pub fn test_set_focus_unchecked(&mut self, column: usize, win: usize) {
        let fr = self.focused_row;
        self.rows[fr].focused_column = column;
        self.rows[fr].focused_window_in_column = win;
    }
}

// Kept nested rather than reindenting the whole suite.
#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use super::*;

    fn placement_geometry(
        placements: Vec<WindowPlacement>,
    ) -> Vec<(WindowId, Rect, Visibility, usize)> {
        placements
            .into_iter()
            .map(|p| (p.window_id, p.rect, p.visibility, p.column_index))
            .collect()
    }

    #[test]
    fn test_center_single_column_modes_scroll_and_default() {
        let viewport = Rect::new(100, 50, 1000, 600);
        for mode in [
            CenteringMode::Center,
            CenteringMode::JustInView,
            CenteringMode::OnOverflow,
        ] {
            for past_edges in [false, true] {
                for width in [400, 960] {
                    let center_x = 100 + (1000 - width) / 2;
                    let mut ws = Workspace::with_gaps(10, 20);
                    ws.set_centering_mode(mode);
                    ws.set_center_past_edges(past_edges);
                    ws.insert_window(1, Some(width)).unwrap();
                    assert_eq!(
                        ws.compute_placements(viewport)[0].rect,
                        Rect::new(120, 70, width, 560)
                    );
                    ws.set_center_single_column(true);
                    for offset in [-100.0, 0.0, 150.0] {
                        ws.set_scroll_offset(offset);
                        ws.reconcile_scroll_bounds(viewport.width);
                        let p = ws.compute_placements(viewport);
                        assert_eq!(p[0].rect, Rect::new(center_x, 70, width, 560));
                        assert_eq!(p[0].visibility, Visibility::Visible);
                        assert_eq!(
                            placement_geometry(ws.compute_placements_animated(viewport)),
                            placement_geometry(p)
                        );
                        assert_eq!(ws.scroll_offset(), -((960 - width) / 2) as f64);
                    }
                    ws.start_scroll_animation(0.0, viewport.width, Some(200), None);
                    ws.tick_animation(50);
                    assert_eq!(ws.compute_placements_animated(viewport)[0].rect.x, center_x);
                    ws.stop_animation();
                    ws.scroll_by(100.0, viewport.width);
                    assert_eq!(ws.compute_placements(viewport)[0].rect.x, center_x);
                    for reduce_motion in [false, true] {
                        ws.set_reduce_motion(reduce_motion);
                        ws.set_scroll_offset(137.0);
                        ws.center_focused_column_animated(viewport.width);
                        ws.stop_animation();
                        assert_eq!(ws.compute_placements(viewport)[0].rect.x, center_x);
                    }
                    ws.set_center_single_column(false);
                    ws.scroll_by(0.0, viewport.width);
                    assert_eq!(ws.compute_placements(viewport)[0].rect.x, 120);
                }
            }
        }
    }

    #[test]
    fn test_center_single_column_transition_preserves_normal_scroll() {
        let viewport = Rect::new(0, 0, 1000, 600);
        for mode in [
            CenteringMode::Center,
            CenteringMode::JustInView,
            CenteringMode::OnOverflow,
        ] {
            for past_edges in [false, true] {
                let mut ws = Workspace::with_gaps(10, 20);
                ws.set_centering_mode(mode);
                ws.set_center_past_edges(past_edges);
                ws.insert_window(1, Some(400)).unwrap();
                ws.set_scroll_offset(137.0);
                ws.set_center_single_column(true);
                ws.ensure_focused_visible(viewport.width);
                assert_eq!(ws.compute_placements(viewport)[0].rect.x, 300);
                assert_eq!(ws.scroll_offset(), -280.0);
                ws.insert_window(2, Some(800)).unwrap();
                let mut normal = ws.clone();
                normal.set_center_single_column(false);
                assert_eq!(ws.compute_placements_animated(viewport)[0].rect.x, 300);
                ws.ensure_focused_visible_animated(viewport.width);
                ws.reconcile_scroll_bounds(viewport.width);
                normal.ensure_focused_visible_animated(viewport.width);
                assert_eq!(ws.compute_placements_animated(viewport)[0].rect.x, 300);
                assert!(ws.is_animating());
                ws.tick_animation(50);
                let intermediate_x = ws.compute_placements_animated(viewport)[0].rect.x;
                assert!(intermediate_x < 300 && intermediate_x > -230);
                ws.stop_animation();
                normal.stop_animation();
                assert_eq!(ws.scroll_offset(), normal.scroll_offset());
                assert_eq!(
                    placement_geometry(ws.compute_placements(viewport)),
                    placement_geometry(normal.compute_placements(viewport))
                );
                assert_eq!(ws.compute_placements_animated(viewport)[0].rect.x, -230);
                ws.remove_window(2).unwrap();
                ws.ensure_focused_visible_animated(viewport.width);
                ws.reconcile_scroll_bounds(viewport.width);
                assert_eq!(ws.compute_placements_animated(viewport)[0].rect.x, -230);
                assert!(ws.is_animating());
                ws.tick_animation(50);
                let intermediate_x = ws.compute_placements_animated(viewport)[0].rect.x;
                assert!(intermediate_x > -230 && intermediate_x < 300);
                assert!(!ws.tick_animation(200));
                assert_eq!(ws.compute_placements_animated(viewport)[0].rect.x, 300);
                assert_eq!(ws.scroll_offset(), -280.0);
                ws.scroll_by(100.0, viewport.width);
                assert_eq!(ws.scroll_offset(), -280.0);
                assert_eq!(ws.compute_placements_animated(viewport)[0].rect.x, 300);
            }
        }
    }

    #[test]
    fn test_center_single_column_reduce_motion_transition() {
        let viewport = Rect::new(0, 0, 1000, 600);
        let mut ws = Workspace::with_gaps(10, 20);
        ws.set_center_single_column(true);
        ws.set_reduce_motion(true);
        ws.insert_window(1, Some(400)).unwrap();
        ws.ensure_focused_visible(viewport.width);
        assert_eq!(ws.compute_placements_animated(viewport)[0].rect.x, 300);
        ws.insert_window(2, Some(800)).unwrap();
        ws.ensure_focused_visible_animated(viewport.width);
        assert!(!ws.is_animating());
        assert_eq!(ws.compute_placements_animated(viewport)[0].rect.x, -230);
        ws.remove_window(2).unwrap();
        ws.ensure_focused_visible_animated(viewport.width);
        assert!(!ws.is_animating());
        assert_eq!(ws.compute_placements_animated(viewport)[0].rect.x, 300);
    }

    #[test]
    fn test_center_single_column_active_stacked_tabbed_and_floating() {
        let viewport = Rect::new(0, 0, 1000, 600);
        let mut ws = Workspace::with_gaps(10, 20);
        ws.set_center_single_column(true);
        ws.insert_window(9, Some(500)).unwrap();
        ws.mark_minimized(9);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.consume_from_left();
        ws.insert_window(10, Some(600)).unwrap();
        ws.mark_minimized(10);
        let float_rect = Rect::new(50, 100, 200, 150);
        ws.add_floating(11, float_rect).unwrap();
        ws.reconcile_scroll_bounds(viewport.width);
        let placements = ws.compute_placements(viewport);
        let stacked: Vec<_> = placements
            .iter()
            .filter(|p| p.column_index != usize::MAX)
            .collect();
        assert_eq!(stacked.len(), 2);
        assert_eq!(stacked[0].rect, Rect::new(300, 20, 400, 275));
        assert_eq!(stacked[1].rect, Rect::new(300, 305, 400, 275));
        assert_eq!(
            placements.iter().find(|p| p.window_id == 11).unwrap().rect,
            float_rect
        );
        ws.focus_window(1).unwrap();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_tab_strip_reserve_px(30);
        let placements = ws.compute_placements(viewport);
        let tab = placements.iter().find(|p| p.window_id == 1).unwrap();
        assert_eq!(tab.rect, Rect::new(300, 50, 400, 530));
        assert_eq!(tab.visibility, Visibility::Visible);
        assert_eq!(
            placements
                .iter()
                .find(|p| p.window_id == 2)
                .unwrap()
                .visibility,
            Visibility::OffScreenLeft
        );
        ws.mark_minimized(1);
        let placements = ws.compute_placements(viewport);
        assert_eq!(
            placements.iter().find(|p| p.window_id == 2).unwrap().rect,
            Rect::new(300, 50, 400, 530)
        );
    }

    #[test]
    fn test_center_single_column_compatibility_boundaries() {
        let viewport = Rect::new(0, 0, 1000, 600);
        for mode in [
            CenteringMode::Center,
            CenteringMode::JustInView,
            CenteringMode::OnOverflow,
        ] {
            for past_edges in [false, true] {
                for widths in [vec![], vec![400, 500], vec![961], vec![400]] {
                    let mut normal = Workspace::with_gaps(10, 20);
                    normal.set_centering_mode(mode);
                    normal.set_center_past_edges(past_edges);
                    for (idx, width) in widths.iter().enumerate() {
                        normal.insert_window(idx as u64 + 1, Some(*width)).unwrap();
                    }
                    if widths == [400] {
                        normal.set_window_min_width(1, 1000);
                    }
                    normal.set_scroll_offset(123.0);
                    let mut centered = normal.clone();
                    centered.set_center_single_column(true);
                    normal.ensure_focused_visible(viewport.width);
                    centered.ensure_focused_visible(viewport.width);
                    assert_eq!(
                        placement_geometry(centered.compute_placements(viewport)),
                        placement_geometry(normal.compute_placements(viewport))
                    );
                    assert_eq!(
                        placement_geometry(centered.compute_placements_animated(viewport)),
                        placement_geometry(normal.compute_placements_animated(viewport))
                    );
                }
            }
        }
        let mut constrained = Workspace::with_gaps(10, 20);
        constrained.insert_window(1, Some(400)).unwrap();
        constrained.set_window_min_width(1, 1000);
        constrained.set_center_single_column(true);
        constrained.ensure_focused_visible(viewport.width);
        constrained.clear_window_min_width(1);
        constrained.reconcile_scroll_bounds(viewport.width);
        assert_eq!(constrained.compute_placements(viewport)[0].rect.x, 300);

        let mut ws = Workspace::with_gaps(10, 20);
        ws.insert_window(1, Some(400)).unwrap();
        ws.toggle_fullscreen();
        let expected = ws.compute_placements(viewport);
        ws.set_center_single_column(true);
        assert_eq!(
            placement_geometry(ws.compute_placements(viewport)),
            placement_geometry(expected.clone())
        );
        assert_eq!(expected[0].rect, viewport);
        ws.toggle_fullscreen();
        ws.mark_minimized(1);
        assert!(ws.compute_placements(viewport).is_empty());
    }

    #[test]
    fn test_center_single_column_nonrevealing_admission() {
        let viewport = Rect::new(0, 0, 1000, 600);
        for sticky in [false, true] {
            let mut ws = Workspace::with_gaps(10, 20);
            ws.set_center_single_column(true);
            ws.insert_window(1, Some(400)).unwrap();
            ws.ensure_focused_visible(viewport.width);
            assert_eq!(ws.compute_placements(viewport)[0].rect.x, 300);
            if sticky {
                ws.append_window_no_focus(2, Some(400)).unwrap();
            } else {
                ws.insert_window_no_focus(2, Some(400)).unwrap();
            }
            ws.reconcile_scroll_bounds(viewport.width);
            for _ in 0..3 {
                let placements = ws.compute_placements(viewport);
                assert_eq!(placements[0].rect.x, 20);
                assert_eq!(placements[1].rect.x, 430);
            }
        }
    }

    #[test]
    fn test_center_single_column_minimize_reconcile() {
        let viewport = Rect::new(0, 0, 1000, 600);
        for animated in [false, true] {
            let mut ws = Workspace::with_gaps(10, 20);
            ws.set_center_single_column(true);
            ws.insert_window(1, Some(400)).unwrap();
            ws.insert_window(2, Some(400)).unwrap();
            ws.focus_window(1).unwrap();
            ws.reconcile_scroll_bounds(viewport.width);
            assert_eq!(ws.compute_placements(viewport)[0].rect.x, 20);
            ws.mark_minimized(2);
            if animated {
                ws.ensure_focused_visible_animated(viewport.width);
                assert!(!ws.tick_animation(1000));
            } else {
                ws.reconcile_scroll_bounds(viewport.width);
            }
            assert_eq!(ws.compute_placements(viewport)[0].rect.x, 300);
            ws.mark_restored(2);
            ws.reconcile_scroll_bounds(viewport.width);
            assert_eq!(ws.compute_placements(viewport)[0].rect.x, 20);
        }
    }

    #[test]
    fn test_center_single_column_migrated_clone_insert() {
        let viewport = Rect::new(0, 0, 1000, 600);
        let mut source = Workspace::with_gaps(10, 20);
        source.set_center_single_column(true);
        source.insert_window(1, Some(400)).unwrap();
        source.ensure_focused_visible(viewport.width);
        assert_eq!(source.compute_placements(viewport)[0].rect.x, 300);
        for query_before_insert in [false, true] {
            let mut migrated = source.clone();
            assert!(migrated.rescale_column_widths(10, 20, 20, 1000, 1400));
            let width = migrated.columns()[0].width();
            if query_before_insert {
                assert_eq!(
                    migrated.compute_placements(Rect::new(0, 0, 1400, 600))[0]
                        .rect
                        .x,
                    (1400 - width) / 2
                );
            }
            migrated.insert_column_at(Column::new(2, 400), 1);
            migrated.reconcile_scroll_bounds(1400);
            let placements = migrated.compute_placements(Rect::new(0, 0, 1400, 600));
            assert_eq!(placements[0].rect.x, 20);
            assert_eq!(placements[1].rect.x, 20 + width + 10);
            assert_eq!(source.compute_placements(viewport)[0].rect.x, 300);
        }
    }

    #[test]
    fn test_center_single_column_remove_column_clamps_during_fullscreen() {
        let viewport = Rect::new(0, 0, 1000, 600);
        let mut ws = Workspace::with_gaps(10, 20);
        ws.set_center_single_column(true);
        ws.insert_window(1, Some(400)).unwrap();
        ws.ensure_focused_visible(viewport.width);
        assert_eq!(ws.scroll_offset(), -280.0);
        ws.insert_window(2, Some(800)).unwrap();
        ws.set_focus(0, 0).unwrap();
        assert!(ws.toggle_fullscreen());
        assert!(ws.remove_column(1).is_some());
        assert!(ws.is_fullscreen());
        assert_eq!(ws.scroll_offset(), 0.0);
    }

    #[test]
    fn test_center_single_column_default_off_scroll_geometry() {
        let viewport = Rect::new(0, 0, 1000, 600);
        let mut default = Workspace::with_gaps(10, 20);
        default.insert_window(1, Some(400)).unwrap();
        default.set_scroll_offset(137.0);
        let mut disabled = default.clone();
        disabled.set_center_single_column(false);
        for ws in [&mut default, &mut disabled] {
            assert_eq!(ws.compute_placements(viewport)[0].rect.x, -117);
            assert_eq!(ws.compute_placements_animated(viewport)[0].rect.x, -117);
            ws.scroll_by(0.0, viewport.width);
            assert_eq!(ws.scroll_offset(), 0.0);
            ws.insert_window(2, Some(800)).unwrap();
            ws.ensure_focused_visible_animated(viewport.width);
            ws.reconcile_scroll_bounds(viewport.width);
            assert_eq!(ws.compute_placements_animated(viewport)[0].rect.x, 20);
            ws.tick_animation(50);
        }
        assert_eq!(
            placement_geometry(default.compute_placements_animated(viewport)),
            placement_geometry(disabled.compute_placements_animated(viewport))
        );
        for ws in [&mut default, &mut disabled] {
            ws.stop_animation();
            assert_eq!(ws.scroll_offset(), 250.0);
            assert_eq!(ws.compute_placements(viewport)[0].rect.x, -230);
        }
    }

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
    fn test_insert_window_at_column_no_focus() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        assert_eq!(ws.focused_column_index(), 1);

        // New column at slot 0 shifts the focused window right; focus follows it.
        ws.insert_window_at_column_no_focus(3, Some(400), 0)
            .unwrap();
        assert_eq!(ws.column_count(), 3);
        assert_eq!(ws.focused_window(), Some(2));
        assert_eq!(ws.focused_column_index(), 2);
        assert!(ws.insert_window_at_column_no_focus(3, None, 0).is_err());

        // Into an empty workspace the new column becomes focused.
        let mut empty = Workspace::new();
        empty.insert_window_at_column_no_focus(9, None, 5).unwrap();
        assert_eq!(empty.focused_window(), Some(9));
    }

    #[test]
    fn test_insert_multiple_windows() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(600)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        assert_eq!(ws.column_count(), 3);
        assert_eq!(ws.focused_column_index(), 2);
        assert_eq!(ws.focused_window(), Some(3));

        // Strip width excludes outer gaps, which pad the viewport.
        assert_eq!(ws.total_width(), 1420);
    }

    #[test]
    fn test_focus_navigation() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        assert_eq!(ws.focused_column_index(), 2);

        ws.focus_left();
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(2));

        ws.focus_left();
        assert_eq!(ws.focused_column_index(), 0);

        ws.focus_left();
        assert_eq!(ws.focused_column_index(), 0);

        ws.focus_right();
        ws.focus_right();
        assert_eq!(ws.focused_column_index(), 2);

        ws.focus_right();
        assert_eq!(ws.focused_column_index(), 2);
    }

    #[test]
    fn test_focus_start_end() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        assert_eq!(ws.focused_column_index(), 2);

        ws.focus_start();
        assert_eq!(ws.focused_column_index(), 0);

        ws.focus_end();
        assert_eq!(ws.focused_column_index(), 2);

        ws.focus_end();
        assert_eq!(ws.focused_column_index(), 2);
        ws.focus_start();
        ws.focus_start();
        assert_eq!(ws.focused_column_index(), 0);

        let mut empty = Workspace::new();
        empty.focus_start();
        empty.focus_end();
    }

    #[test]
    fn test_move_column_to_start_end() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        assert_eq!(ws.focused_column_index(), 2);
        assert_eq!(ws.focused_window(), Some(3));

        ws.move_column_to_start();
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(3));

        ws.move_column_to_end();
        assert_eq!(ws.focused_column_index(), 2);
        assert_eq!(ws.focused_window(), Some(3));

        ws.move_column_to_end();
        assert_eq!(ws.focused_column_index(), 2);
        let mut empty = Workspace::new();
        empty.move_column_to_start();
        empty.move_column_to_end();
    }

    #[test]
    fn test_move_window_past_edge_unstacks() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.move_window_left();
        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.focused_column_index(), 0);

        // Moving right at the right edge unstacks into a new column off the end.
        ws.move_window_right();
        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.focused_window(), Some(2));
        assert_eq!(ws.focused_column_index(), 1);

        // A lone window at the edge stays put (nothing to unstack).
        ws.move_window_right();
        assert_eq!(ws.column_count(), 2);

        let mut empty = Workspace::new();
        empty.move_window_left();
        empty.move_window_right();
    }

    #[test]
    fn test_move_window_left_edge_unstacks() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.move_window_left();
        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.focused_column_index(), 0);

        // Moving left at the left edge unstacks into a new column at the edge.
        ws.move_window_left();
        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.focused_window(), Some(2));
        assert_eq!(ws.focused_column_index(), 0);

        // A lone window at the left edge stays put (nothing to unstack).
        ws.move_window_left();
        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.focused_column_index(), 0);
    }

    #[test]
    fn test_append_window_no_focus() {
        let mut ws = Workspace::new();
        ws.append_window_no_focus(1, Some(400)).unwrap();
        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.focused_column_index(), 0);

        ws.insert_window(2, Some(400)).unwrap();
        ws.focus_start();
        assert_eq!(ws.focused_column_index(), 0);

        ws.append_window_no_focus(3, Some(400)).unwrap();
        assert_eq!(ws.column_count(), 3);
        assert_eq!(ws.focused_column_index(), 0);

        assert!(ws.append_window_no_focus(3, Some(400)).is_err());
        assert_eq!(ws.column_count(), 3);
        assert_eq!(ws.focused_column_index(), 0);

        ws.focus_end();
        assert_eq!(ws.focused_window(), Some(3));
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

        assert!(ws.columns().iter().any(|c| c.contains(1)));
        assert!(ws.columns().iter().any(|c| c.contains(3)));
    }

    #[test]
    fn test_compute_placements_visibility() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        ws.set_scroll_offset(0.0);

        let viewport = Rect::new(0, 0, 500, 600);
        let placements = ws.compute_placements(viewport);

        assert_eq!(placements.len(), 3);

        assert_eq!(placements[0].visibility, Visibility::Visible);
        assert_eq!(placements[0].window_id, 1);

        // Second column partially visible
        assert_eq!(placements[1].visibility, Visibility::Visible);

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
        ws.set_scroll_offset(500.0);

        ws.ensure_focused_visible(500);

        // Centering needs an offset of -40, which clamps to zero.
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
        assert!(placements.iter().all(|p| p.column_index == 0));
    }

    #[test]
    fn test_column_edge_detection() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        assert!(ws.at_column_top());
        assert!(ws.at_column_bottom());

        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();
        assert_eq!(ws.columns()[0].len(), 3);

        ws.set_focus(0, 0).unwrap();
        assert!(ws.at_column_top());
        assert!(!ws.at_column_bottom());

        ws.set_focus(0, 1).unwrap();
        assert!(!ws.at_column_top());
        assert!(!ws.at_column_bottom());

        ws.set_focus(0, 2).unwrap();
        assert!(!ws.at_column_top());
        assert!(ws.at_column_bottom());

        // Multiple tabs cycle instead of exposing a navigation edge.
        ws.toggle_focused_column_tabbed_mode();
        ws.set_focus(0, 0).unwrap();
        assert!(!ws.at_column_top());
        assert!(!ws.at_column_bottom());
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
    fn test_move_column_skips_minimized_column_and_restores_order() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        ws.mark_minimized(2);
        ws.test_set_focus_unchecked(2, 0);

        ws.move_column_left();

        assert_eq!(ws.columns()[0].windows(), &[3]);
        assert_eq!(ws.columns()[1].windows(), &[2]);
        assert_eq!(ws.columns()[2].windows(), &[1]);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(3));

        ws.move_column_right();

        assert_eq!(ws.columns()[0].windows(), &[1]);
        assert_eq!(ws.columns()[1].windows(), &[2]);
        assert_eq!(ws.columns()[2].windows(), &[3]);
        assert_eq!(ws.focused_column_index(), 2);
        assert_eq!(ws.focused_window(), Some(3));
    }

    #[test]
    fn test_move_column_noop_beyond_active_edge() {
        for move_left in [true, false] {
            let mut ws = Workspace::new();
            ws.insert_window(1, Some(400)).unwrap();
            ws.insert_window(2, Some(400)).unwrap();
            ws.insert_window(3, Some(400)).unwrap();
            ws.mark_minimized(1);
            ws.mark_minimized(3);
            ws.test_set_focus_unchecked(1, 0);

            if move_left {
                ws.move_column_left();
            } else {
                ws.move_column_right();
            }

            assert_eq!(ws.columns()[0].windows(), &[1]);
            assert_eq!(ws.columns()[1].windows(), &[2]);
            assert_eq!(ws.columns()[2].windows(), &[3]);
            assert_eq!(ws.focused_column_index(), 1);
            assert_eq!(ws.focused_window(), Some(2));
        }
    }

    #[test]
    fn test_move_column_skips_minimized_column_to_mixed_column() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(4, 0).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        ws.mark_minimized(1);
        ws.mark_minimized(2);
        ws.test_set_focus_unchecked(2, 0);

        ws.move_column_left();

        assert_eq!(ws.columns()[0].windows(), &[3]);
        assert_eq!(ws.columns()[1].windows(), &[2]);
        assert_eq!(ws.columns()[2].windows(), &[1, 4]);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(3));
    }

    #[test]
    fn test_reorder_column_forward() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        ws.test_set_focus_unchecked(0, 0);

        ws.reorder_column(0, 2);
        assert_eq!(ws.columns()[0].get(0), Some(2));
        assert_eq!(ws.columns()[1].get(0), Some(3));
        assert_eq!(ws.columns()[2].get(0), Some(1));
        assert_eq!(ws.focused_column_index(), 2);
    }

    #[test]
    fn test_reorder_column_backward() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        ws.test_set_focus_unchecked(2, 0);

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
        ws.test_set_focus_unchecked(1, 0);

        // Moving an earlier column past the focused column shifts its index left.
        ws.reorder_column(0, 2);
        assert_eq!(ws.focused_column_index(), 0);
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
        // Removing an earlier column decrements the focus index without changing its window.
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

        let col = Column::new(99, 300);
        ws.insert_column_at(col, 0);
        assert_eq!(ws.column_count(), 3);
        assert_eq!(ws.columns()[0].get(0), Some(99));
        assert_eq!(ws.columns()[1].get(0), Some(1));
        assert_eq!(ws.columns()[2].get(0), Some(2));
        assert_eq!(ws.focused_column_index(), 1);
    }

    #[test]
    fn test_insert_column_at_end() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.test_set_focus_unchecked(0, 0);

        let col = Column::new(99, 300);
        ws.insert_column_at(col, 100);
        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[1].get(0), Some(99));
        assert_eq!(ws.focused_column_index(), 0);
    }

    #[test]
    fn test_scroll_by() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        let viewport_width = 500;
        let vis_w = viewport_width - 10 - 10;

        ws.scroll_by(100.0, viewport_width);
        assert_eq!(ws.scroll_offset(), 100.0);

        ws.scroll_by(2000.0, viewport_width);
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

        ws.insert_window(1, Some(200)).unwrap();
        ws.insert_window(2, Some(200)).unwrap();
        ws.insert_window(3, Some(200)).unwrap();

        ws.test_set_focus_unchecked(0, 0);
        ws.set_scroll_offset(0.0);

        ws.ensure_focused_visible(500);
        assert_eq!(ws.scroll_offset(), 0.0);

        // Focus column 2, which is partially out of view
        ws.test_set_focus_unchecked(2, 0);
        ws.ensure_focused_visible(500);
        assert_eq!(ws.scroll_offset(), 140.0);
    }

    #[test]
    fn test_on_overflow_narrow_column_behaves_just_in_view() {
        let offset_for = |mode| {
            let mut ws = Workspace::with_gaps(10, 10);
            ws.set_centering_mode(mode);
            ws.insert_window(1, Some(200)).unwrap();
            ws.insert_window(2, Some(200)).unwrap();
            ws.insert_window(3, Some(200)).unwrap();
            // The visible middle column distinguishes just-in-view from centering.
            ws.test_set_focus_unchecked(1, 0);
            ws.set_scroll_offset(0.0);
            ws.ensure_focused_visible(500);
            ws.scroll_offset()
        };
        assert_eq!(
            offset_for(CenteringMode::OnOverflow),
            offset_for(CenteringMode::JustInView)
        );
        assert_ne!(
            offset_for(CenteringMode::OnOverflow),
            offset_for(CenteringMode::Center)
        );
    }

    #[test]
    fn test_on_overflow_centers_wide_column() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.set_centering_mode(CenteringMode::OnOverflow);
        ws.insert_window(1, Some(1500)).unwrap();
        ws.test_set_focus_unchecked(0, 0);

        // Centering the oversized column requires an offset of (1500 - 1000) / 2 = 250.
        ws.ensure_focused_visible(1000);
        assert_eq!(ws.scroll_offset(), 250.0);
    }

    #[test]
    fn test_min_width_constraint_rederives_focused_visibility_for_centering_modes() {
        let viewport = Rect::new(0, 0, 1000, 600);

        for mode in [
            CenteringMode::Center,
            CenteringMode::JustInView,
            CenteringMode::OnOverflow,
        ] {
            let mut ws = Workspace::with_gaps(0, 0);
            ws.set_outer_gaps(100, 100, 0, 0);
            ws.set_centering_mode(mode);
            ws.insert_window(1, Some(400)).unwrap();
            ws.insert_window(2, Some(400)).unwrap();
            ws.insert_window(3, Some(300)).unwrap();
            ws.test_set_focus_unchecked(2, 0);

            assert_eq!(ws.visible_width(viewport.width), 800);
            ws.set_window_min_width(3, 900);
            assert_eq!(ws.columns()[2].width(), 300);
            assert_eq!(ws.effective_column_width(&ws.columns()[2]), 900);
            assert_eq!(ws.total_width(), 1700);
            assert_eq!(
                ws.compute_placements(viewport)[2].visibility,
                Visibility::OffScreenRight,
                "{mode:?} begins with the widened focused column outside the visible strip"
            );

            ws.ensure_focused_visible(viewport.width);
            assert_eq!(
                ws.compute_placements(viewport)[2].visibility,
                Visibility::Visible,
                "{mode:?} re-derives the widened focused column into view"
            );
        }
    }

    #[test]
    fn test_compute_placements_tight_viewport() {
        let mut ws = Workspace::with_gaps(10, 50);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();

        let viewport = Rect::new(0, 0, 500, 80);
        let placements = ws.compute_placements(viewport);

        assert_eq!(placements.len(), 3);
        assert_eq!(
            placements.iter().map(|p| p.window_id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        for p in &placements {
            assert!(p.rect.height >= 0, "Height was negative: {}", p.rect.height);
        }
    }

    #[test]
    fn test_insert_window_clamps_width() {
        let mut ws = Workspace::new();

        ws.insert_window(1, Some(0)).unwrap();
        assert_eq!(ws.columns()[0].width(), 100);

        ws.insert_window(2, Some(-50)).unwrap();
        assert_eq!(ws.columns()[1].width(), 100);
    }

    #[test]
    fn test_scroll_offset_rounding() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        ws.set_scroll_offset(100.7);

        let viewport = Rect::new(0, 0, 500, 600);
        let placements = ws.compute_placements(viewport);

        // Rounding 100.7 to 101 places the first column at 10 - 101 = -91.
        assert_eq!(placements[0].rect.x, -91);
    }

    #[test]
    fn test_column_empty_constructor() {
        let col = Column::empty(50);
        assert_eq!(col.width(), 100);
        assert!(col.is_empty());
        assert_eq!(col.len(), 0);

        let col = Column::empty(300);
        assert_eq!(col.width(), 300);
        assert_eq!(col.windows(), &[] as &[WindowId]);
    }

    #[test]
    fn test_rect_right_and_bottom() {
        let r = Rect::new(10, 20, 100, 50);
        assert_eq!(r.right(), 110);
        assert_eq!(r.bottom(), 70);

        let r2 = Rect::new(-50, -30, 100, 80);
        assert_eq!(r2.right(), 50);
        assert_eq!(r2.bottom(), 50);
    }

    #[test]
    fn test_focus_operations_on_empty_workspace() {
        let mut ws = Workspace::new();

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

        ws.test_set_focus_unchecked(2, 0);

        ws.remove_window(1).unwrap();

        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(3));
    }

    #[test]
    fn test_remove_window_clamps_focus_index_after_column_removal() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        ws.insert_window_in_column(4, 2).unwrap();
        ws.insert_window_in_column(5, 2).unwrap();

        // Simulate stale focus index before removing a column.
        ws.test_set_focus_unchecked(1, 99);
        ws.remove_window(2).unwrap();

        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window_index_in_column(), 2);
        assert_eq!(ws.focused_window(), Some(5));
    }

    #[test]
    fn test_duplicate_window_rejected() {
        let mut ws = Workspace::new();
        ws.insert_window(42, Some(400)).unwrap();

        let result = ws.insert_window(42, Some(400));
        assert!(matches!(result, Err(LayoutError::DuplicateWindow(42))));

        let result = ws.insert_window_in_column(42, 0);
        assert!(matches!(result, Err(LayoutError::DuplicateWindow(42))));

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

        for i in 0..1000 {
            ws.insert_window(i, Some(i32::MAX / 100)).unwrap();
        }

        let width = ws.total_width();
        assert!(width > 0);
        assert_eq!(width, i32::MAX);
    }

    #[test]
    fn test_focus_window_by_id() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        assert_eq!(ws.focused_window(), Some(3));

        ws.focus_window(1).unwrap();
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(1));

        ws.focus_window(2).unwrap();
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(2));

        let result = ws.focus_window(999);
        assert!(matches!(result, Err(LayoutError::WindowNotFound(999))));
    }

    #[test]
    fn test_set_focus_validates() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window_in_column(3, 1).unwrap();

        ws.set_focus(1, 1).unwrap();
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window_index_in_column(), 1);
        assert_eq!(ws.focused_window(), Some(3));

        let result = ws.set_focus(5, 0);
        assert!(matches!(result, Err(LayoutError::ColumnOutOfBounds(5, 1))));

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

        ws.scroll_by(50.0, viewport_width);
        assert_eq!(ws.scroll_offset(), 50.0);

        ws.scroll_by(f64::NAN, viewport_width);
        assert_eq!(ws.scroll_offset(), 50.0);

        ws.scroll_by(f64::INFINITY, viewport_width);
        assert_eq!(ws.scroll_offset(), 50.0);

        ws.scroll_by(f64::NEG_INFINITY, viewport_width);
        assert_eq!(ws.scroll_offset(), 50.0);
    }

    #[test]
    fn test_column_width_getter() {
        let col = Column::new(1, 500);
        assert_eq!(col.width(), 500);

        let col2 = Column::new(2, 50);
        assert_eq!(col2.width(), 100);
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

        assert_eq!(col.get(0), Some(1));
        assert_eq!(col.get(1), Some(2));
        assert_eq!(col.get(2), Some(3));
        assert_eq!(col.get(10), None);

        assert_eq!(col.windows(), &[1, 2, 3]);
    }

    #[test]
    fn test_remove_window_before_focus_in_stacked_column() {
        // Removing an earlier stacked window changes the focus index, not the focused window.
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();

        ws.test_set_focus_unchecked(0, 1);
        assert_eq!(ws.focused_window(), Some(2));

        ws.remove_window(1).unwrap();

        assert_eq!(ws.focused_window(), Some(2));
        assert_eq!(ws.focused_window_index_in_column(), 0);
    }

    #[test]
    fn test_remove_focused_window_in_stacked_column() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();

        ws.test_set_focus_unchecked(0, 1);
        assert_eq!(ws.focused_window(), Some(2));

        ws.remove_window(2).unwrap();

        assert_eq!(ws.focused_window(), Some(3));
        assert_eq!(ws.focused_window_index_in_column(), 1);
    }

    #[test]
    fn test_remove_last_focused_window_in_stacked_column() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();

        ws.test_set_focus_unchecked(0, 2);
        assert_eq!(ws.focused_window(), Some(3));

        ws.remove_window(3).unwrap();

        assert_eq!(ws.focused_window(), Some(2));
        assert_eq!(ws.focused_window_index_in_column(), 1);
    }

    #[test]
    fn test_compute_placements_wide_column() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(1000)).unwrap();

        let viewport = Rect::new(0, 0, 500, 600);
        let placements = ws.compute_placements(viewport);

        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].visibility, Visibility::Visible);
        assert_eq!(placements[0].rect.width, 1000);
    }

    #[test]
    fn test_negative_gaps_clamped() {
        let ws = Workspace::with_gaps(-100, -50);
        assert_eq!(ws.gap(), 0);
        assert_eq!(ws.outer_gaps(), (0, 0, 0, 0));
    }

    #[test]
    fn test_gap_setters_clamp() {
        let mut ws = Workspace::new();

        ws.set_gap(20);
        assert_eq!(ws.gap(), 20);
        ws.set_gap(-50);
        assert_eq!(ws.gap(), 0);

        ws.set_outer_gaps(15, 15, 15, 15);
        assert_eq!(ws.outer_gaps(), (15, 15, 15, 15));
        ws.set_outer_gaps(-100, -100, -100, -100);
        assert_eq!(ws.outer_gaps(), (0, 0, 0, 0));

        ws.set_default_column_width(500);
        assert_eq!(ws.default_column_width(), 500);
        ws.set_default_column_width(50);
        assert_eq!(ws.default_column_width(), 100);
    }

    #[test]
    fn test_compute_placements_spacing_integrity() {
        let mut ws = Workspace::with_gaps(10, 20);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();

        let viewport = Rect::new(0, 0, 500, 600);
        let placements = ws.compute_placements(viewport);

        let total_height: i32 = placements.iter().map(|p| p.rect.height).sum();
        let (_, _, outer_top, outer_bottom) = ws.outer_gaps();
        let expected_usable = viewport.height - outer_top - outer_bottom;
        let expected_gaps = ws.gap() * (placements.len() as i32 - 1);

        assert_eq!(total_height + expected_gaps, expected_usable);
    }

    #[test]
    fn test_column_remove_returns_index() {
        let mut col = Column::new(1, 400);
        col.add_window(2);
        col.add_window(3);

        let removed = col.remove_window(2);
        assert_eq!(removed, Some(1));

        let removed = col.remove_window(1);
        assert_eq!(removed, Some(0));

        let removed = col.remove_window(999);
        assert_eq!(removed, None);
    }

    #[test]
    fn test_compute_placements_zero_viewport_width() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        let viewport = Rect::new(0, 0, 0, 600);
        let placements = ws.compute_placements(viewport);

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

        let viewport = Rect::new(0, 0, 500, 0);
        let placements = ws.compute_placements(viewport);

        assert_eq!(placements.len(), 2);
        for p in &placements {
            assert!(p.rect.height >= 0, "Height was negative: {}", p.rect.height);
        }
    }

    #[test]
    fn test_focus_navigation_clamps_to_shorter_column() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();
        ws.insert_window_in_column(4, 0).unwrap();

        ws.test_set_focus_unchecked(0, 2);
        assert_eq!(ws.focused_window(), Some(4));

        ws.focus_right();

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

        ws.test_set_focus_unchecked(2, 0);
        ws.resize_focused_column(500);
        assert_eq!(ws.columns()[2].width(), 700);

        ws.ensure_focused_visible(500);
        assert_eq!(ws.scroll_offset(), 640.0);
        let placements = ws.compute_placements(Rect::new(0, 0, 500, 600));
        let focused = placements.iter().find(|p| p.window_id == 3).unwrap();
        assert_eq!(focused.visibility, Visibility::Visible);
    }

    #[test]
    fn test_cycle_width_up_with_ensure_keeps_rightmost_focused_in_view() {
        // Width cycling needs a visibility reconciliation to keep the grown column on-screen.
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_centering_mode(CenteringMode::Center);
        ws.insert_window(1, Some(300)).unwrap();
        ws.insert_window(2, Some(300)).unwrap();
        ws.insert_window(3, Some(300)).unwrap();

        let vw = 700;
        ws.test_set_focus_unchecked(2, 0);
        ws.ensure_focused_visible(vw);

        let presets = vec![0.333, 0.5, 0.667];
        ws.cycle_width_up(&presets, vw);
        ws.ensure_focused_visible(vw);

        let placements = ws.compute_placements(Rect::new(0, 0, vw, 1000));
        let focused = placements
            .iter()
            .find(|p| p.window_id == 3)
            .expect("focused window present");
        assert_eq!(
            focused.visibility,
            Visibility::Visible,
            "grown focused column stays visible, not off-screen right"
        );
        assert!(
            focused.rect.right() <= vw,
            "focused column right edge stays within the viewport (was {} > {})",
            focused.rect.right(),
            vw
        );
    }

    #[test]
    fn test_move_column_then_resize() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(300)).unwrap();
        ws.insert_window(3, Some(500)).unwrap();

        ws.test_set_focus_unchecked(1, 0);
        assert_eq!(ws.columns()[1].width(), 300);

        ws.move_column_left();
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.columns()[0].width(), 300);

        ws.resize_focused_column(100);
        assert_eq!(ws.columns()[0].width(), 400);
    }

    #[test]
    fn test_remove_reinsert_same_window_id() {
        let mut ws = Workspace::new();
        ws.insert_window(42, Some(400)).unwrap();
        ws.insert_window(100, Some(300)).unwrap();

        ws.remove_window(42).unwrap();
        assert!(!ws.contains_window(42));

        ws.insert_window(42, Some(500)).unwrap();
        assert!(ws.contains_window(42));
        assert_eq!(ws.focused_window(), Some(42));
    }

    #[test]
    fn test_find_window_location() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();
        ws.insert_window_in_column(4, 1).unwrap();

        assert_eq!(ws.find_window_location_rc(1), Some((0, 0, 0)));
        assert_eq!(ws.find_window_location_rc(2), Some((0, 1, 0)));
        assert_eq!(ws.find_window_location_rc(3), Some((0, 0, 1)));
        assert_eq!(ws.find_window_location_rc(4), Some((0, 1, 1)));
        assert_eq!(ws.find_window_location_rc(999), None);
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

        ws.resize_focused_column(100);
        ws.resize_focused_column(-100);
        ws.resize_focused_column(i32::MAX);
        ws.resize_focused_column(i32::MIN);

        assert!(ws.is_empty());
    }

    #[test]
    fn test_invariants_after_complex_sequence() {
        let mut ws = Workspace::with_gaps(10, 10);

        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(300)).unwrap();
        ws.insert_window(3, Some(500)).unwrap();
        ws.insert_window_in_column(4, 1).unwrap();
        ws.insert_window_in_column(5, 1).unwrap();

        ws.focus_left();
        ws.focus_down();
        ws.move_column_right();
        ws.resize_focused_column(100);
        ws.focus_right();
        ws.focus_up();
        ws.remove_window(4).unwrap();
        ws.focus_left();
        ws.move_column_left();

        assert!(ws.focused_column_index() < ws.columns().len());
        assert!(ws.focused_window_in_column() < ws.columns()[ws.focused_column_index()].len());

        let mut all_windows: Vec<WindowId> = ws
            .columns()
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
        assert_ne!(col1, col3);
        assert_ne!(col1, col4);
    }

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
        let ease_out = Easing::EaseOut;

        assert!((ease_out.apply(0.0) - 0.0).abs() < f64::EPSILON);
        assert!((ease_out.apply(1.0) - 1.0).abs() < f64::EPSILON);

        for (t, expected) in [(0.25, 0.578125), (0.5, 0.875), (0.75, 0.984375)] {
            assert!((ease_out.apply(t) - expected).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn test_easing_ease_in() {
        let ease_in = Easing::EaseIn;

        assert!((ease_in.apply(0.0) - 0.0).abs() < f64::EPSILON);
        assert!((ease_in.apply(1.0) - 1.0).abs() < f64::EPSILON);

        for (t, expected) in [(0.25, 0.015625), (0.5, 0.125), (0.75, 0.421875)] {
            assert!((ease_in.apply(t) - expected).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn test_easing_ease_in_out() {
        let ease_in_out = Easing::EaseInOut;

        assert!((ease_in_out.apply(0.0) - 0.0).abs() < f64::EPSILON);
        assert!((ease_in_out.apply(1.0) - 1.0).abs() < f64::EPSILON);
        assert!((ease_in_out.apply(0.5) - 0.5).abs() < f64::EPSILON);

        assert!(ease_in_out.apply(0.25) < 0.25);
        assert!(ease_in_out.apply(0.75) > 0.75);
    }

    #[test]
    fn test_easing_clamps_input() {
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
    fn test_scroll_animation_progress() {
        let mut anim = ScrollAnimation::new(0.0, 100.0, 100, Easing::Linear);

        assert!((anim.progress() - 0.0).abs() < f64::EPSILON);

        anim.elapsed_ms = 50;
        assert!((anim.progress() - 0.5).abs() < f64::EPSILON);

        anim.elapsed_ms = 100;
        assert!((anim.progress() - 1.0).abs() < f64::EPSILON);

        anim.elapsed_ms = 150;
        assert!((anim.progress() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_scroll_animation_progress_zero_duration() {
        let anim = ScrollAnimation::new(0.0, 100.0, 0, Easing::Linear);
        assert!((anim.progress() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_scroll_animation_current_offset_linear() {
        let mut anim = ScrollAnimation::new(0.0, 100.0, 100, Easing::Linear);

        assert!((anim.current_offset() - 0.0).abs() < f64::EPSILON);

        anim.elapsed_ms = 50;
        assert!((anim.current_offset() - 50.0).abs() < f64::EPSILON);

        anim.elapsed_ms = 100;
        assert!((anim.current_offset() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_scroll_animation_current_offset_eased() {
        let mut anim = ScrollAnimation::new(0.0, 100.0, 100, Easing::EaseOut);

        anim.elapsed_ms = 50;
        assert!((anim.current_offset() - 87.5).abs() < 1e-9);
    }

    #[test]
    fn test_scroll_animation_negative_direction() {
        let mut anim = ScrollAnimation::new(100.0, 0.0, 100, Easing::Linear);

        assert!((anim.current_offset() - 100.0).abs() < f64::EPSILON);

        anim.elapsed_ms = 50;
        assert!((anim.current_offset() - 50.0).abs() < f64::EPSILON);

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

        assert!(anim.tick(25));
        assert_eq!(anim.elapsed_ms, 25);

        assert!(anim.tick(25));
        assert_eq!(anim.elapsed_ms, 50);

        assert!(anim.tick(25));
        assert_eq!(anim.elapsed_ms, 75);

        assert!(!anim.tick(25));
        assert_eq!(anim.elapsed_ms, 100);

        assert!(!anim.tick(50));
        assert_eq!(anim.elapsed_ms, 150);
    }

    #[test]
    fn test_scroll_animation_tick_saturating() {
        let mut anim = ScrollAnimation::new(0.0, 100.0, 100, Easing::Linear);

        anim.elapsed_ms = u64::MAX - 10;
        anim.tick(100);
        assert_eq!(anim.elapsed_ms, u64::MAX);
    }

    #[test]
    fn test_workspace_is_animating() {
        let mut ws = Workspace::with_gaps(10, 10);
        for i in 1..=5 {
            ws.insert_window(i, Some(400)).unwrap();
        }

        assert!(!ws.is_animating());

        ws.start_scroll_animation(100.0, 500, None, None);
        assert!(ws.is_animating());

        ws.tick_animation(300);
        assert!(!ws.is_animating());
    }

    #[test]
    fn test_workspace_effective_scroll_offset() {
        let mut ws = Workspace::with_gaps(10, 10);
        for i in 1..=5 {
            ws.insert_window(i, Some(400)).unwrap();
        }

        assert!((ws.effective_scroll_offset() - 0.0).abs() < 1.0);

        ws.start_scroll_animation(200.0, 500, Some(100), Some(Easing::Linear));
        assert!(ws.is_animating());

        assert!(ws.effective_scroll_offset() < 10.0);

        ws.tick_animation(50);
        assert!(ws.effective_scroll_offset() > 80.0 && ws.effective_scroll_offset() < 120.0);
    }

    #[test]
    fn test_workspace_start_scroll_animation_clamps_target() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();

        ws.start_scroll_animation(500.0, 1000, None, None);

        assert!(!ws.is_animating());
    }

    #[test]
    fn test_workspace_tick_animation() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        ws.start_scroll_animation(200.0, 500, Some(100), Some(Easing::Linear));

        assert!(ws.tick_animation(30));
        assert!(ws.is_animating());

        assert!(!ws.tick_animation(100));
        assert!(!ws.is_animating());

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

        assert!(ws.is_animating());
        assert!((ws.effective_scroll_offset() - 100.0).abs() < 1e-9);

        ws.cancel_animation();
        assert!(!ws.is_animating());
        assert!((ws.effective_scroll_offset() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn test_workspace_animation_no_effect_when_at_target() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();

        ws.start_scroll_animation(0.0, 1000, None, None);
        assert!(!ws.is_animating());
    }

    #[test]
    fn test_workspace_animation_interrupt() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        ws.start_scroll_animation(200.0, 500, Some(100), Some(Easing::Linear));
        ws.tick_animation(50);

        assert!((ws.effective_scroll_offset() - 100.0).abs() < 1e-9);

        ws.start_scroll_animation(300.0, 500, Some(100), Some(Easing::Linear));
        assert!(ws.is_animating());
        assert!((ws.effective_scroll_offset() - 100.0).abs() < 1e-9);

        ws.tick_animation(50);
        assert!((ws.effective_scroll_offset() - 200.0).abs() < 1e-9);

        ws.tick_animation(50);
        assert!(!ws.is_animating());
        assert!((ws.effective_scroll_offset() - 300.0).abs() < 1e-9);
    }

    #[test]
    fn test_compute_placements_animated() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        let viewport = Rect::new(0, 0, 500, 600);

        let placements1 = ws.compute_placements_animated(viewport);
        assert_eq!(placements1.len(), 2);
        assert_eq!(placements1[0].window_id, 1);
        assert_eq!(placements1[0].rect.x, 10);
        assert_eq!(placements1[0].visibility, Visibility::Visible);

        ws.start_scroll_animation(200.0, 500, Some(100), Some(Easing::Linear));
        ws.tick_animation(50);
        assert!(ws.is_animating());

        let placements2 = ws.compute_placements_animated(viewport);
        assert_eq!(placements2.len(), 2);
        assert_eq!(placements2[0].window_id, 1);
        assert_eq!(placements2[0].rect.x, -90);
        assert_eq!(placements2[0].visibility, Visibility::Visible);
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
        for i in 1..=5 {
            ws.insert_window(i, Some(400)).unwrap();
        }

        ws.ensure_focused_visible(500);

        ws.focus_left();
        ws.focus_left();
        ws.focus_left();
        ws.focus_left();

        ws.ensure_focused_visible_animated(500);

        assert!(ws.is_animating());
        ws.tick_animation(DEFAULT_ANIMATION_DURATION_MS);

        ws.set_focus(2, 0).unwrap();
        ws.ensure_focused_visible_animated(500);
        assert!(ws.is_animating());
        ws.tick_animation(DEFAULT_ANIMATION_DURATION_MS);
        assert!(!ws.is_animating());
        assert_eq!(ws.scroll_offset(), 780.0);
        let placements = ws.compute_placements_animated(Rect::new(0, 0, 500, 600));
        let focused = placements.iter().find(|p| p.window_id == 3).unwrap();
        assert_eq!(focused.rect.x, 50);
        assert_eq!(focused.visibility, Visibility::Visible);
    }

    #[test]
    fn test_add_floating_window() {
        let mut ws = Workspace::new();
        let rect = Rect::new(100, 100, 400, 300);

        ws.add_floating(1, rect).unwrap();

        assert!(ws.contains_window(1));
        assert!(ws.is_floating(1));
        assert_eq!(ws.floating_count(), 1);
        assert_eq!(ws.column_count(), 0);
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

        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

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

        ws.insert_window(1, Some(400)).unwrap();

        let result = ws.add_floating(1, Rect::new(100, 100, 400, 300));
        assert!(matches!(result, Err(LayoutError::DuplicateWindow(1))));
    }

    #[test]
    fn test_fullscreen_toggle() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();

        assert!(!ws.is_fullscreen());

        let entered = ws.toggle_fullscreen();
        assert!(entered);
        assert!(ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), Some(1));

        let entered = ws.toggle_fullscreen();
        assert!(!entered);
        assert!(!ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), None);
    }

    #[test]
    fn test_fullscreen_follow_focus() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.toggle_fullscreen();
        assert_eq!(ws.fullscreen_window_id(), Some(2));

        ws.focus_left();
        assert!(ws.fullscreen_follow_focus(), "fullscreen retargeted");
        assert!(ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), Some(1));

        assert!(!ws.fullscreen_follow_focus());

        ws.toggle_fullscreen();
        assert!(!ws.is_fullscreen());
        assert!(!ws.fullscreen_follow_focus());
    }

    #[test]
    fn test_fullscreen_exit_clears_min_width() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();

        ws.toggle_fullscreen();
        assert!(ws.is_fullscreen());

        // Simulate a resize constraint measured at fullscreen viewport size.
        ws.set_window_min_width(1, 1920);

        let entered = ws.toggle_fullscreen();
        assert!(!entered);

        assert_eq!(ws.columns()[0].width(), 400);

        assert_eq!(ws.effective_column_width(&ws.columns()[0]), 400);
    }

    #[test]
    fn test_min_height_pins_first_window_in_stacked_column() {
        // The flexible window receives the height left after satisfying the pinned minimum.
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();

        ws.set_window_min_height(1, 800);

        // Available height is 1000 - 20 outer gaps - 10 inter-window gap = 970.
        let viewport = Rect::new(0, 0, 800, 1000);
        let placements = ws.compute_placements(viewport);
        let visible: Vec<_> = placements
            .iter()
            .filter(|p| p.visibility == Visibility::Visible)
            .collect();

        assert_eq!(visible.len(), 2);
        let pinned = visible.iter().find(|p| p.window_id == 1).unwrap();
        let flexible = visible.iter().find(|p| p.window_id == 2).unwrap();

        assert!(
            pinned.rect.height >= 800,
            "pinned window should get at least 800px, got {}",
            pinned.rect.height
        );
        // The last window absorbs the remaining height and rounding remainder.
        assert!(flexible.rect.height >= 0);
        let bottom = flexible.rect.y + flexible.rect.height;
        assert_eq!(bottom, viewport.y + viewport.height - 10);
    }

    #[test]
    fn test_min_height_pins_last_window_in_stacked_column() {
        // The last window's minimum must also constrain its rounding remainder.
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();

        // Available height is 960; the two flexible shares round to 380 each,
        // leaving 200, one pixel below the last window's minimum.
        ws.set_window_min_height(3, 201);

        let viewport = Rect::new(0, 0, 800, 1000);
        let placements = ws.compute_placements(viewport);
        let last = placements.iter().find(|p| p.window_id == 3).unwrap();
        assert_eq!(last.rect.height, 201);
    }

    #[test]
    fn test_min_height_oversubscribed_column_overflows_gracefully() {
        // Minimum heights take precedence over fitting the column inside the viewport.
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();

        // Available height is 600 - 20 outer gaps - 10 inter-window gap = 570.
        ws.set_window_min_height(1, 700);
        ws.set_window_min_height(2, 700);

        let viewport = Rect::new(0, 0, 800, 600);
        let placements = ws.compute_placements(viewport);
        let w1 = placements.iter().find(|p| p.window_id == 1).unwrap();
        let w2 = placements.iter().find(|p| p.window_id == 2).unwrap();

        assert!(
            w1.rect.height >= 700,
            "window 1 should get its 700px minimum, got {}",
            w1.rect.height
        );
        assert!(
            w2.rect.height >= 700,
            "window 2 should get its 700px minimum, got {}",
            w2.rect.height
        );
    }

    #[test]
    fn test_min_height_cleared_on_fullscreen_exit() {
        // Fullscreen-size measurements are not constraints for the normal viewport.
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();

        ws.toggle_fullscreen();
        assert!(ws.is_fullscreen());

        ws.set_window_min_height(1, 1080);

        let entered = ws.toggle_fullscreen();
        assert!(!entered);

        let viewport = Rect::new(0, 0, 800, 600);
        let placements = ws.compute_placements(viewport);
        let p = placements.iter().find(|p| p.window_id == 1).unwrap();
        assert!(p.rect.height < 1080);
    }

    #[test]
    fn test_toggle_fullscreen_targets_visible_window_when_focus_is_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();

        // Stacking preserves focus on window 1; minimizing it exposes window 2 as the fallback.
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

        ws.focus_left();
        assert_eq!(ws.focused_window(), Some(2));
        ws.toggle_fullscreen();

        let placements = ws.compute_placements(viewport);
        assert_eq!(placements.len(), 3);

        let fs = placements.iter().find(|p| p.window_id == 2).unwrap();
        assert_eq!(fs.rect, viewport);
        assert_eq!(fs.visibility, Visibility::Visible);

        let w1 = placements.iter().find(|p| p.window_id == 1).unwrap();
        assert_eq!(w1.visibility, Visibility::OffScreenLeft);

        let w3 = placements.iter().find(|p| p.window_id == 3).unwrap();
        assert_eq!(w3.visibility, Visibility::OffScreenLeft);
    }

    #[test]
    fn test_fullscreen_placements_keep_real_rects() {
        let mut ws = Workspace::with_gaps(10, 10);
        let viewport = Rect::new(0, 0, 1920, 1080);
        let float_rect = Rect::new(50, 60, 300, 200);

        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.add_floating(10, float_rect).unwrap();

        let _ = ws.focus_window(1);
        ws.toggle_fullscreen();
        assert_eq!(ws.fullscreen_window_id(), Some(1));

        let placements = ws.compute_placements(viewport);

        let fs = placements.iter().find(|p| p.window_id == 1).unwrap();
        assert_eq!(fs.rect, viewport);
        assert_eq!(fs.visibility, Visibility::Visible);

        let w2 = placements.iter().find(|p| p.window_id == 2).unwrap();
        assert_ne!(w2.visibility, Visibility::Visible);
        assert!(
            w2.rect.width > 0,
            "tiled rect width stays real, got {:?}",
            w2.rect
        );
        assert!(
            w2.rect.height > 0,
            "tiled rect height stays real, got {:?}",
            w2.rect
        );

        let fl = placements.iter().find(|p| p.window_id == 10).unwrap();
        assert_ne!(fl.visibility, Visibility::Visible);
        assert_eq!(fl.rect, float_rect, "floating rect stays real");
    }

    #[test]
    fn test_pinned_floating_stays_visible_under_fullscreen() {
        let mut ws = Workspace::with_gaps(10, 10);
        let viewport = Rect::new(0, 0, 1920, 1080);
        let float_rect = Rect::new(50, 60, 300, 200);

        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.add_floating(10, float_rect).unwrap();
        assert!(ws.set_floating_pinned(10, true));

        let _ = ws.focus_window(1);
        ws.toggle_fullscreen();
        assert_eq!(ws.fullscreen_window_id(), Some(1));

        let placements = ws.compute_placements(viewport);
        let fl = placements.iter().find(|p| p.window_id == 10).unwrap();
        assert_eq!(
            fl.visibility,
            Visibility::Visible,
            "pinned floating stays visible"
        );
        assert_eq!(fl.rect, float_rect, "pinned floating keeps its rect");
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

    #[test]
    fn test_toggle_fullscreen_window_floating_fills_viewport() {
        let mut ws = Workspace::with_gaps(10, 10);
        let viewport = Rect::new(0, 0, 1920, 1080);
        let float_rect = Rect::new(50, 60, 300, 200);
        let pinned_rect = Rect::new(700, 60, 200, 150);

        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.add_floating(10, float_rect).unwrap();
        ws.add_floating(11, Rect::new(400, 60, 200, 150)).unwrap();
        ws.add_floating(12, pinned_rect).unwrap();
        assert!(ws.set_floating_pinned(12, true));

        assert!(ws.toggle_fullscreen_window(10));
        assert_eq!(ws.fullscreen_window_id(), Some(10));

        let placements = ws.compute_placements(viewport);
        let fs = placements.iter().find(|p| p.window_id == 10).unwrap();
        assert_eq!(fs.rect, viewport);
        assert_eq!(fs.visibility, Visibility::Visible);
        for id in [1, 2, 11] {
            let p = placements.iter().find(|p| p.window_id == id).unwrap();
            assert_eq!(p.visibility, Visibility::OffScreenLeft, "window {id}");
        }
        let pinned = placements.iter().find(|p| p.window_id == 12).unwrap();
        assert_eq!(pinned.visibility, Visibility::Visible);
        assert_eq!(pinned.rect, pinned_rect);
    }

    #[test]
    fn test_toggle_fullscreen_window_floating_exit_restores_rect() {
        let mut ws = Workspace::new();
        let viewport = Rect::new(0, 0, 1920, 1080);
        let float_rect = Rect::new(50, 60, 300, 200);

        ws.insert_window(1, Some(400)).unwrap();
        ws.add_floating(10, float_rect).unwrap();

        assert!(ws.toggle_fullscreen_window(10));
        ws.set_window_min_width(10, 1920);
        ws.set_window_min_height(10, 1080);

        assert!(!ws.toggle_fullscreen_window(10));
        assert!(!ws.is_fullscreen());
        assert!(ws.is_floating(10));
        assert_eq!(ws.focused_window(), Some(1));

        let placements = ws.compute_placements(viewport);
        let fl = placements.iter().find(|p| p.window_id == 10).unwrap();
        assert_eq!(fl.rect, float_rect);
        assert_eq!(fl.visibility, Visibility::Visible);
        assert!(!ws.window_min_widths.contains_key(&10));
        assert!(!ws.window_min_heights.contains_key(&10));
    }

    #[test]
    fn test_toggle_fullscreen_window_retargets_from_another_window() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.add_floating(10, Rect::new(50, 60, 300, 200)).unwrap();

        assert!(ws.toggle_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), Some(1));
        ws.set_window_min_width(1, 1920);

        assert!(ws.toggle_fullscreen_window(10));
        assert_eq!(ws.fullscreen_window_id(), Some(10));
        assert!(!ws.window_min_widths.contains_key(&1));
    }

    #[test]
    fn test_toggle_fullscreen_window_rejects_missing_or_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.add_floating(10, Rect::new(50, 60, 300, 200)).unwrap();
        ws.mark_minimized(10);

        assert!(!ws.toggle_fullscreen_window(10));
        assert!(!ws.toggle_fullscreen_window(999));
        assert!(!ws.is_fullscreen());

        // A rejected target leaves an existing fullscreen window in place.
        assert!(ws.toggle_fullscreen());
        assert!(!ws.toggle_fullscreen_window(999));
        assert_eq!(ws.fullscreen_window_id(), Some(1));
    }

    #[test]
    fn test_resize_floating_changes_width_only() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.add_floating(10, Rect::new(50, 60, 300, 200)).unwrap();

        assert!(ws.resize_floating(10, 100));
        assert_eq!(ws.floating_windows()[0].rect, Rect::new(50, 60, 400, 200));

        assert!(ws.resize_floating(10, -250));
        assert_eq!(ws.floating_windows()[0].rect, Rect::new(50, 60, 150, 200));

        assert_eq!(ws.columns()[0].width(), 400);
    }

    #[test]
    fn test_resize_floating_clamps_to_min_width() {
        let mut ws = Workspace::new();
        ws.add_floating(10, Rect::new(50, 60, 300, 200)).unwrap();
        ws.add_floating(11, Rect::new(50, 60, 80, 200)).unwrap();

        assert!(ws.resize_floating(10, -1000));
        assert_eq!(ws.floating_windows()[0].rect.width, MIN_COLUMN_WIDTH);

        // A window already below the floor does not grow when shrunk.
        assert!(ws.resize_floating(11, -50));
        assert_eq!(ws.floating_windows()[1].rect.width, 80);
        assert!(ws.resize_floating(11, 50));
        assert_eq!(ws.floating_windows()[1].rect.width, 130);
    }

    #[test]
    fn test_resize_floating_ignores_tiled_and_missing_windows() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();

        assert!(!ws.resize_floating(1, 100));
        assert!(!ws.resize_floating(999, 100));
        assert_eq!(ws.columns()[0].width(), 400);
    }

    #[test]
    fn test_toggle_floating_tiled_to_float() {
        let mut ws = Workspace::new();
        let viewport = Rect::new(0, 0, 1920, 1080);

        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        let wid = ws.toggle_floating(viewport);
        assert_eq!(wid, Some(2));
        assert!(ws.is_floating(2));
        assert_eq!(ws.column_count(), 1);
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

        let wid = ws.toggle_floating(viewport);
        assert_eq!(wid, Some(1));
        assert!(ws.is_floating(1));
        assert_eq!(ws.column_count(), 0);

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

    #[test]
    fn test_set_column_width_fraction() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();

        // Width fractions include a gap in the 1910px base, then subtract that gap.
        ws.set_focused_column_width_fraction(0.5, 1920);
        assert_eq!(ws.columns()[0].width(), 945);

        ws.set_focused_column_width_fraction(0.333, 1920);
        // floor(0.333 * 1910 - 10)
        assert_eq!(ws.columns()[0].width(), 626);
    }

    #[test]
    fn test_set_column_width_fraction_clamp() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();

        ws.set_focused_column_width_fraction(2.0, 1920);
        let w = ws.columns()[0].width();
        assert_eq!(w, 1900);
    }

    #[test]
    fn test_equalize_widths() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(300)).unwrap();
        ws.insert_window(2, Some(600)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        // Each width is (1920 - 20 outer gaps - 20 inter-column gaps) / 3 = 626.
        ws.equalize_column_widths(1920);

        assert_eq!(ws.columns()[0].width(), 626);
        assert_eq!(ws.columns()[1].width(), 626);
        assert_eq!(ws.columns()[2].width(), 626);
    }

    #[test]
    fn test_equalize_widths_empty() {
        let mut ws = Workspace::new();
        ws.equalize_column_widths(1920);
    }

    #[test]
    fn test_native_minimum_geometry_agrees_across_placement_and_focus_paths() {
        for mode in [
            CenteringMode::Center,
            CenteringMode::JustInView,
            CenteringMode::OnOverflow,
        ] {
            let mut ws = Workspace::with_gaps(10, 10);
            ws.set_centering_mode(mode);
            ws.insert_window(1, Some(467)).unwrap();
            ws.insert_window(2, Some(467)).unwrap();
            ws.insert_window(3, Some(467)).unwrap();
            ws.set_window_min_width(2, 842);
            assert_eq!(ws.total_width(), 1796);
            let full = ws.placements_for_full_strip(Rect::new(0, 0, 1816, 600));
            assert_eq!(
                full.iter()
                    .map(|p| (p.rect.x, p.rect.width))
                    .collect::<Vec<_>>(),
                vec![(10, 467), (487, 842), (1339, 467)]
            );
            ws.focus_window(2).unwrap();
            let viewport = Rect::new(0, 0, 1000, 600);
            ws.ensure_focused_visible(viewport.width);
            let placements = ws.compute_placements(viewport);
            assert!(placements[1].rect.x >= 10);
            assert!(placements[1].rect.x + placements[1].rect.width <= 990);
            assert_eq!(
                ws.compute_placements_animated(viewport)
                    .iter()
                    .map(|p| (p.window_id, p.rect))
                    .collect::<Vec<_>>(),
                placements
                    .iter()
                    .map(|p| (p.window_id, p.rect))
                    .collect::<Vec<_>>()
            );
            assert_eq!(ws.columns()[1].width(), 467);
        }
    }

    #[test]
    fn test_native_minimum_clear_retargets_scroll_when_current_focus_already_fits() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_centering_mode(CenteringMode::JustInView);
        ws.insert_window(1, Some(467)).unwrap();
        ws.insert_window(2, Some(467)).unwrap();
        ws.insert_window(3, Some(467)).unwrap();
        ws.focus_window(2).unwrap();
        ws.set_window_min_width(3, 842);
        ws.set_scroll_offset(100.0);
        ws.start_scroll_animation(816.0, 1000, Some(1000), None);
        ws.tick_animation(10);
        let current = ws.effective_scroll_offset();
        ws.clear_window_min_width(3);
        ws.ensure_focused_visible_animated(1000);
        assert!((ws.effective_scroll_offset() - current).abs() < 0.5);
        ws.tick_animation(1000);
        assert_eq!(ws.scroll_offset(), 441.0);
    }

    #[test]
    fn test_native_minimum_clear_keeps_animated_centering_past_edges() {
        for reduce_motion in [false, true] {
            let mut ws = Workspace::with_gaps(10, 10);
            ws.set_centering_mode(CenteringMode::Center);
            ws.set_center_past_edges(true);
            ws.set_reduce_motion(reduce_motion);
            ws.insert_window(1, Some(467)).unwrap();
            ws.insert_window(2, Some(1000)).unwrap();
            ws.focus_window(1).unwrap();
            ws.set_window_min_width(1, 842);
            ws.ensure_focused_visible(1000);
            assert_eq!(ws.scroll_offset(), -69.0);
            ws.clear_window_min_width(1);
            ws.ensure_focused_visible_animated(1000);
            assert_eq!(
                ws.effective_scroll_offset(),
                if reduce_motion { -257.0 } else { -69.0 }
            );
            ws.tick_animation(1000);
            assert_eq!(ws.scroll_offset(), -257.0);
        }
    }

    #[test]
    fn test_native_minimum_clear_preserves_explicit_centering() {
        for mode in [CenteringMode::JustInView, CenteringMode::OnOverflow] {
            for reduce_motion in [false, true] {
                let mut ws = Workspace::with_gaps(10, 10);
                ws.set_centering_mode(mode);
                ws.set_center_past_edges(true);
                ws.set_reduce_motion(false);
                ws.insert_window(1, Some(467)).unwrap();
                ws.insert_window(2, Some(1000)).unwrap();
                ws.focus_window(1).unwrap();
                ws.set_window_min_width(1, 842);
                ws.center_focused_column_animated(1000);
                ws.tick_animation(10);
                let before = ws.effective_scroll_offset();
                ws.clear_window_min_width(1);
                ws.set_reduce_motion(reduce_motion);
                ws.ensure_focused_visible_animated(1000);
                if !reduce_motion {
                    assert_eq!(ws.effective_scroll_offset(), before);
                }
                ws.tick_animation(1000);
                assert_eq!(ws.scroll_offset(), -69.0, "{mode:?}, {reduce_motion}");
            }
        }
    }

    #[test]
    fn test_reconcile_scroll_bounds_preserves_valid_scroll_and_animation() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_centering_mode(CenteringMode::Center);
        for id in [1, 2, 3] {
            ws.insert_window(id, Some(800)).unwrap();
        }
        ws.set_scroll_offset(137.0);
        ws.reconcile_scroll_bounds(1920);
        assert_eq!(ws.scroll_offset(), 137.0);
        ws.start_scroll_animation(400.0, 1920, Some(1000), Some(Easing::Linear));
        ws.tick_animation(100);
        let mut expected = ws.clone();
        ws.reconcile_scroll_bounds(1920);
        assert!(ws.is_animating());
        assert_eq!(
            ws.effective_scroll_offset(),
            expected.effective_scroll_offset()
        );
        ws.tick_animation(100);
        expected.tick_animation(100);
        assert_eq!(
            ws.effective_scroll_offset(),
            expected.effective_scroll_offset()
        );
    }

    #[test]
    fn test_reconcile_scroll_bounds_cancels_invalid_current_or_target() {
        for (current, target) in [(1220.0, 300.0), (100.0, 1220.0)] {
            let mut ws = Workspace::with_gaps(10, 10);
            ws.set_center_past_edges(false);
            for id in [1, 2, 3] {
                ws.insert_window(id, Some(800)).unwrap();
            }
            ws.set_window_min_width(3, 1500);
            ws.set_scroll_offset(current);
            ws.start_scroll_animation(target, 1920, Some(1000), Some(Easing::Linear));
            ws.tick_animation(10);
            let expected = ws.effective_scroll_offset().clamp(0.0, 520.0);
            ws.clear_window_min_width(3);
            ws.reconcile_scroll_bounds(1920);
            assert!(!ws.is_animating());
            assert_eq!(ws.scroll_offset(), expected);
            ws.tick_animation(1000);
            assert_eq!(ws.scroll_offset(), expected);
        }
    }

    #[test]
    fn test_reconcile_scroll_bounds_honors_effective_edge_centering() {
        for (focused, centered) in [(2, -450.0), (4, 1620.0)] {
            for animated in [false, true] {
                for allow_edges in [false, true] {
                    let mut ws = Workspace::with_gaps(10, 10);
                    ws.set_center_past_edges(true);
                    ws.set_reduce_motion(!animated);
                    for id in 1..=5 {
                        ws.insert_window(id, Some(if id == 1 || id == 5 { 5000 } else { 800 }))
                            .unwrap();
                    }
                    ws.mark_minimized(1);
                    ws.mark_minimized(5);
                    ws.set_window_min_width(2, 1000);
                    ws.set_window_min_width(4, 1500);
                    ws.focus_window(focused).unwrap();
                    ws.center_focused_column_animated(1920);
                    if animated {
                        ws.tick_animation(10);
                    }
                    let current = ws.effective_scroll_offset();
                    ws.set_center_past_edges(allow_edges);
                    ws.reconcile_scroll_bounds(1920);
                    if allow_edges {
                        assert_eq!(ws.is_animating(), animated);
                        assert_eq!(ws.effective_scroll_offset(), current);
                    } else {
                        assert!(!ws.is_animating());
                        assert_eq!(ws.scroll_offset(), current.clamp(0.0, 1420.0));
                    }
                    ws.tick_animation(1000);
                    assert_eq!(
                        ws.scroll_offset(),
                        if allow_edges {
                            centered
                        } else {
                            current.clamp(0.0, 1420.0)
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn test_reconcile_scroll_bounds_empty_or_minimized_strip() {
        for minimized in [false, true] {
            let mut ws = Workspace::with_gaps(10, 10);
            ws.set_center_past_edges(true);
            if minimized {
                ws.insert_window(1, Some(800)).unwrap();
                ws.mark_minimized(1);
            }
            ws.set_scroll_offset(100.0);
            ws.reconcile_scroll_bounds(1920);
            assert_eq!(ws.scroll_offset(), 0.0);
            assert!(!ws.is_animating());
        }
    }

    #[test]
    fn test_native_minimum_resize_uses_effective_baseline_except_zero() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(467)).unwrap();
        ws.set_window_min_width(1, 842);
        ws.set_window_min_height(1, 700);
        ws.resize_focused_column(0);
        assert_eq!(ws.columns()[0].width(), 467);
        assert_eq!(ws.window_min_widths.get(&1), Some(&842));
        assert_eq!(ws.window_min_heights.get(&1), Some(&700));
        for (delta, expected) in [(10, 852), (-10, 832), (i32::MIN, MIN_COLUMN_WIDTH)] {
            let mut resized = ws.clone();
            resized.resize_focused_column(delta);
            assert_eq!(resized.columns()[0].width(), expected);
            assert!(resized.window_min_widths.is_empty());
            assert!(resized.window_min_heights.is_empty());
        }
        ws.toggle_maximize_column(1920);
        ws.resize_focused_column(0);
        assert!(!ws.toggle_maximize_column(1920));
        assert_eq!(ws.columns()[0].width(), 467);
    }

    #[test]
    fn test_native_minimum_presets_and_maximized_restore_keep_requested_intent() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(467)).unwrap();
        ws.set_window_min_width(1, 842);
        let presets = [0.25, 0.5, 0.75];
        ws.cycle_width_up(&presets, 1000);
        assert_eq!(
            ws.columns()[0].width(),
            467,
            "no upward preset above the effective width"
        );
        ws.cycle_width_down(&presets, 1000);
        assert_eq!(ws.columns()[0].width(), 732);
        assert_eq!(ws.effective_column_width(&ws.columns()[0]), 842);
        ws.set_focused_column_width_fraction(0.25, 1000);
        assert_eq!(ws.columns()[0].width(), 237);
        assert_eq!(ws.effective_column_width(&ws.columns()[0]), 842);
        ws.snap_column_width_to_preset(0, 850, &presets, 1920);
        assert_eq!(ws.columns()[0].width(), 945);
        assert!(ws.window_min_widths.is_empty());

        ws.set_all_column_widths(1267);
        ws.set_window_min_width(1, 1500);
        assert!(ws.toggle_maximize_column(5120));
        ws.rescale_column_widths(10, 10, 10, 5120, 2560);
        assert!(!ws.toggle_maximize_column(2560));
        assert_eq!(ws.columns()[0].width(), 627);
        assert_eq!(ws.effective_column_width(&ws.columns()[0]), 1500);
        ws.equalize_column_widths(1000);
        assert_eq!(ws.columns()[0].width(), 980);
        assert!(ws.window_min_widths.is_empty());
    }

    #[test]
    fn test_native_minimum_lifecycle_preserves_requests_and_serialization() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(467)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.commit_pending_min_size_clears();
        ws.set_window_min_width(1, 842);
        ws.set_window_min_width(2, 700);
        let saved = serde_json::to_value(&ws).unwrap();
        let col_width = saved
            .get("columns")
            .and_then(|c| c[0]["width"].as_i64())
            .or_else(|| saved["rows"][0]["columns"][0]["width"].as_i64());
        assert_eq!(col_width, Some(467));
        assert!(saved.get("window_min_widths").is_none());
        let restored: Workspace = serde_json::from_value(saved).unwrap();
        assert_eq!(restored.effective_column_width(&restored.columns()[0]), 467);
        assert!(ws.mark_minimized(1));
        assert_eq!(ws.effective_column_width(&ws.columns()[0]), 467);
        assert!(ws.mark_restored(1));
        assert_eq!(ws.effective_column_width(&ws.columns()[0]), 467);
        ws.set_window_min_width(1, 842);
        ws.insert_window_in_column(3, 0).unwrap();
        assert_eq!(ws.effective_column_width(&ws.columns()[0]), 842);
        assert!(ws.commit_pending_min_size_clears());
        assert_eq!(ws.effective_column_width(&ws.columns()[0]), 467);
    }

    #[test]
    fn test_rescale_column_widths_for_viewport_change() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(945)).unwrap();

        assert!(ws.rescale_column_widths(10, 10, 10, 1920, 1280));
        assert_eq!(ws.columns()[0].width(), 625);

        assert!(ws.rescale_column_widths(10, 10, 10, 1280, 1920));
        assert_eq!(ws.columns()[0].width(), 945);
    }

    #[test]
    fn test_rescale_column_widths_round_trip_tolerates_rounding() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(626)).unwrap();
        let original = ws.columns()[0].width();

        ws.rescale_column_widths(10, 10, 10, 1920, 1365);
        ws.rescale_column_widths(10, 10, 10, 1365, 1920);

        assert!((ws.columns()[0].width() - original).abs() <= 1);
    }

    #[test]
    fn test_rescale_column_widths_combines_viewport_and_gap_changes() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(945)).unwrap();
        ws.set_gap(20);
        ws.set_outer_gaps(20, 20, 10, 10);

        assert!(ws.rescale_column_widths(10, 10, 10, 1920, 1280));
        assert_eq!(ws.columns()[0].width(), 610);
    }

    #[test]
    fn test_rescale_column_widths_preserves_ticket_round_trip() {
        fn rescale(ws: &mut Workspace, old_vw: i32, new_vw: i32, new_gap: i32) {
            let old_gap = ws.gap();
            let (old_ol, old_or, _, _) = ws.outer_gaps();
            ws.set_gap(new_gap);
            ws.set_outer_gaps(new_gap, new_gap, new_gap, new_gap);
            assert!(ws.rescale_column_widths(old_gap, old_ol, old_or, old_vw, new_vw,));
        }

        let mut halves = Workspace::with_gaps(10, 10);
        halves.insert_window(1, Some(400)).unwrap();
        halves.insert_window(2, Some(400)).unwrap();
        halves.set_focus(0, 0).unwrap();
        halves.set_focused_column_width_fraction(0.5, 5120);
        halves.set_focus(1, 0).unwrap();
        halves.set_focused_column_width_fraction(0.5, 5120);
        let original_halves: Vec<_> = halves.columns().iter().map(|c| c.width()).collect();
        assert_eq!(original_halves, [2545, 2545]);

        let mut quarters = Workspace::with_gaps(10, 10);
        for id in 1..=4 {
            quarters.insert_window(id, Some(400)).unwrap();
            quarters.set_focused_column_width_fraction(0.25, 5120);
        }
        let original_quarters: Vec<_> = quarters.columns().iter().map(|c| c.width()).collect();
        assert_eq!(original_quarters, [1267; 4]);

        for ws in [&mut halves, &mut quarters] {
            rescale(ws, 5120, 5120, 13);
            rescale(ws, 5120, 1920, 13);
            rescale(ws, 1920, 1280, 10);
            rescale(ws, 1280, 5120, 10);
        }

        let final_halves: Vec<_> = halves.columns().iter().map(|c| c.width()).collect();
        assert_eq!(final_halves, original_halves);
        assert_eq!(final_halves.iter().sum::<i32>() + halves.gap(), 5100);
        assert!(halves.total_width() <= 5120 - 20);

        let final_quarters: Vec<_> = quarters.columns().iter().map(|c| c.width()).collect();
        assert_eq!(final_quarters, original_quarters);
        assert!(final_quarters.iter().sum::<i32>() + quarters.gap() * 3 <= 5100);
        assert!(quarters.total_width() <= 5120 - 20);
    }

    #[test]
    fn test_rescale_column_widths_preserves_round_trip_after_expel() {
        let mut restored_widths = Vec::new();
        for expel_left in [true, false] {
            let mut ws = Workspace::with_gaps(10, 10);
            ws.insert_window(1, Some(1267)).unwrap();
            ws.insert_window_in_column(2, 0).unwrap();
            ws.set_focus(0, 1).unwrap();

            assert!(ws.rescale_column_widths(10, 10, 10, 5120, 1280));
            assert_eq!(ws.columns()[0].width(), 307);
            if expel_left {
                ws.expel_to_left();
            } else {
                ws.expel_to_right();
            }
            assert_eq!(ws.column_count(), 2);

            assert!(ws.rescale_column_widths(10, 10, 10, 1280, 5120));
            restored_widths.push(ws.columns().iter().map(|c| c.width()).collect::<Vec<_>>());
        }
        assert_eq!(restored_widths, [[1267; 2]; 2]);
    }

    #[test]
    fn test_rescale_column_widths_uses_pixel_width_after_cross_workspace_move() {
        let mut source = Workspace::with_gaps(10, 10);
        source.insert_window(1, Some(400)).unwrap();
        source.set_focused_column_width_fraction(0.5, 5120);
        source.set_gap(13);
        source.set_outer_gaps(13, 13, 13, 13);
        assert!(source.rescale_column_widths(10, 10, 10, 5120, 5120));

        let mut destination = Workspace::with_gaps(10, 10);
        destination.insert_window(2, Some(400)).unwrap();
        destination.insert_column_at(source.remove_column(0).unwrap(), 0);
        let width_before_rescale = destination.columns()[0].width();

        destination.set_gap(13);
        destination.set_outer_gaps(13, 13, 13, 13);
        assert!(destination.rescale_column_widths(10, 10, 10, 1920, 1920));

        let old_base = 1920 - 10 - 10 + 10;
        let new_base = 1920 - 13 - 13 + 13;
        let expected = ((width_before_rescale + 10) as f64 / old_base as f64 * new_base as f64
            - 13.0)
            .round() as i32;
        assert_eq!(destination.columns()[0].width(), expected);
    }

    #[test]
    fn test_rescale_column_widths_respects_manual_resize_after_cache() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(600)).unwrap();
        assert!(ws.rescale_column_widths(10, 10, 10, 1920, 1280));

        ws.resize_focused_column(100);
        let resized_width = ws.columns()[0].width();
        assert_eq!(resized_width, 496);

        assert!(ws.rescale_column_widths(10, 10, 10, 1280, 1920));
        let expected = ((1920 - 20 + 10) as f64 * (resized_width + 10) as f64
            / (1280 - 20 + 10) as f64
            - 10.0)
            .round() as i32;
        assert_eq!(ws.columns()[0].width(), expected);
        assert_ne!(ws.columns()[0].width(), 600);
    }

    #[test]
    fn test_rescale_column_widths_maximized_restore_round_trip() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(1267)).unwrap();
        let original_width = ws.columns()[0].width();
        assert!(ws.toggle_maximize_column(5120));

        fn rescale(ws: &mut Workspace, old_vw: i32, new_vw: i32, new_gap: i32) {
            let old_gap = ws.gap();
            let (old_ol, old_or, _, _) = ws.outer_gaps();
            ws.set_gap(new_gap);
            ws.set_outer_gaps(new_gap, new_gap, new_gap, new_gap);
            assert!(ws.rescale_column_widths(old_gap, old_ol, old_or, old_vw, new_vw,));
        }

        rescale(&mut ws, 5120, 5120, 13);
        rescale(&mut ws, 5120, 1920, 13);
        rescale(&mut ws, 1920, 1280, 10);
        rescale(&mut ws, 1280, 5120, 10);
        assert!(!ws.toggle_maximize_column(5120));
        assert_eq!(ws.columns()[0].width(), original_width);
    }

    #[test]
    fn test_rescale_column_widths_noop_preserves_scroll_animation() {
        let mut ws = Workspace::with_gaps(10, 10);
        for id in 1..=4 {
            ws.insert_window(id, Some(400)).unwrap();
        }
        ws.set_scroll_offset(100.0);
        ws.start_scroll_animation(500.0, 800, Some(100), Some(Easing::Linear));
        ws.tick_animation(50);
        let effective_before = ws.effective_scroll_offset();
        let stored_before = ws.scroll_offset();

        assert!(!ws.rescale_column_widths(10, 10, 10, 800, 800));
        assert!(ws.is_animating());
        assert_eq!(ws.scroll_offset(), stored_before);
        assert!((ws.effective_scroll_offset() - effective_before).abs() < 1.0);
    }

    #[test]
    fn test_rescale_column_widths_clamps_minimum_and_scroll() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(100)).unwrap();
        ws.insert_window(2, Some(100)).unwrap();
        ws.set_scroll_offset(1000.0);

        assert!(ws.rescale_column_widths(10, 10, 10, 1920, 500));
        assert_eq!(ws.columns()[0].width(), MIN_COLUMN_WIDTH);
        assert_eq!(ws.columns()[1].width(), MIN_COLUMN_WIDTH);
        assert_eq!(ws.scroll_offset(), 0.0);
    }

    #[test]
    fn test_rescale_column_widths_preserves_maximized_restore_fraction() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(600)).unwrap();
        assert!(ws.toggle_maximize_column(1920));
        assert_eq!(ws.columns()[0].width(), 1900);

        assert!(ws.rescale_column_widths(10, 10, 10, 1920, 1280));
        assert_eq!(ws.columns()[0].width(), 1260);

        assert!(!ws.toggle_maximize_column(1280));
        assert_eq!(ws.columns()[0].width(), 396);
    }

    #[test]
    fn test_rescale_column_widths_cancels_animation_at_current_position() {
        let mut ws = Workspace::with_gaps(10, 10);
        for id in 1..=3 {
            ws.insert_window(id, Some(400)).unwrap();
        }
        ws.start_scroll_animation(700.0, 500, Some(100), Some(Easing::Linear));
        ws.tick_animation(50);
        let current = ws.effective_scroll_offset();

        assert!(ws.rescale_column_widths(10, 10, 10, 500, 300));
        assert!(!ws.is_animating());
        assert!((ws.scroll_offset() - current).abs() < 1.0);
    }

    #[test]
    fn test_unfloat_nonexistent() {
        let mut ws = Workspace::new();
        assert!(!ws.unfloat_window(999));
    }

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
        assert!(ws.mark_minimized(1));
        assert!(ws.is_minimized(1));
    }

    #[test]
    fn test_mark_minimized_idempotent() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        assert!(ws.mark_minimized(1));
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
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();

        let viewport = Rect::new(0, 0, 400, 1000);
        let before = ws.compute_placements(viewport);
        assert_eq!(before.len(), 2);
        assert_eq!(before[0].rect.height, 500);
        assert_eq!(before[1].rect.height, 500);

        ws.mark_minimized(1);
        let after = ws.compute_placements(viewport);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].window_id, 2);
        assert_eq!(after[0].rect.height, 1000);
    }

    #[test]
    fn test_minimize_all_in_column() {
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
        assert!(ws.contains_window(1));
    }

    #[test]
    fn test_minimized_window_count_unchanged() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        assert_eq!(ws.window_count(), 2);

        ws.mark_minimized(1);
        assert_eq!(ws.window_count(), 2);
    }

    #[test]
    fn test_fullscreen_minimize_clears_fullscreen() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        let _ = ws.focus_window(1);
        ws.toggle_fullscreen();
        assert!(ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), Some(1));

        ws.mark_minimized(1);
        assert!(!ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), None);

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

        let _ = ws.focus_window(1);
        ws.toggle_fullscreen();
        assert!(ws.is_fullscreen());

        ws.mark_minimized(1);
        assert!(!ws.is_fullscreen());

        ws.mark_restored(1);
        assert!(!ws.is_fullscreen());
    }

    #[test]
    fn test_minimize_non_fullscreen_window_keeps_fullscreen() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        let _ = ws.focus_window(1);
        ws.toggle_fullscreen();
        assert!(ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), Some(1));

        ws.mark_minimized(2);
        assert!(ws.is_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), Some(1));
    }

    #[test]
    fn test_focus_left_skips_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        assert_eq!(ws.focused_window(), Some(3));

        ws.mark_minimized(2);

        ws.focus_left();
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_focus_right_skips_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        let _ = ws.focus_window(1);
        assert_eq!(ws.focused_window(), Some(1));

        ws.mark_minimized(2);

        ws.focus_right();
        assert_eq!(ws.focused_window(), Some(3));
    }

    #[test]
    fn test_focus_down_skips_minimized_in_stack() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();

        let _ = ws.focus_window(1);
        assert_eq!(ws.focused_window(), Some(1));

        ws.mark_minimized(2);

        ws.focus_down();
        assert_eq!(ws.focused_window(), Some(3));
    }

    #[test]
    fn test_focus_up_skips_minimized_in_stack() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();

        let _ = ws.focus_window(3);
        assert_eq!(ws.focused_window(), Some(3));

        ws.mark_minimized(2);

        ws.focus_up();
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_focus_left_into_mixed_column_lands_on_visible() {
        // Enter a stacked column whose first window is minimized.
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window(3, Some(400)).unwrap();

        ws.mark_minimized(1);

        let _ = ws.focus_window(3);
        assert_eq!(ws.focused_window(), Some(3));

        ws.focus_left();
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_focus_right_into_mixed_column_lands_on_visible() {
        // Enter a stacked column whose first window is minimized.
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window_in_column(3, 1).unwrap();

        ws.mark_minimized(2);

        let _ = ws.focus_window(1);
        assert_eq!(ws.focused_window(), Some(1));

        ws.focus_right();
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

        let _ = ws.focus_window(1);
        ws.mark_minimized(1);

        assert_eq!(ws.focused_window(), Some(1));

        assert_eq!(ws.focused_visible_window(), Some(2));
    }

    #[test]
    fn test_focused_visible_window_all_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.mark_minimized(1);

        assert_eq!(ws.focused_visible_window(), None);
    }

    #[test]
    fn test_minimize_clamps_stale_scroll_just_in_view() {
        // The focused column can appear in view even when scroll exceeds the reduced maximum.
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_centering_mode(CenteringMode::JustInView);

        // 3 columns of 800px: total_width = 2420, visible width = 1920 - 20 = 1900
        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window(2, Some(800)).unwrap();
        ws.insert_window(3, Some(800)).unwrap();

        let vp_width = 1920;

        let _ = ws.focus_window(3);
        ws.ensure_focused_visible(vp_width);
        // max_scroll = 2420 - 1900 = 520
        assert_eq!(ws.scroll_offset() as i32, 520);

        ws.mark_minimized(3);
        ws.focus_right();
        ws.focus_left();
        assert_eq!(ws.focused_window(), Some(2));

        ws.ensure_focused_visible_animated(vp_width);

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
        // A surviving middle column must not retain the scroll needed before minimization.
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_centering_mode(CenteringMode::JustInView);

        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window(2, Some(800)).unwrap();
        ws.insert_window(3, Some(800)).unwrap();

        let viewport = Rect::new(0, 0, 1920, 1080);
        let vp_width = viewport.width;

        let _ = ws.focus_window(3);
        ws.ensure_focused_visible(vp_width);

        ws.mark_minimized(1);
        ws.mark_minimized(3);
        let _ = ws.focus_window(2);

        ws.ensure_focused_visible_animated(vp_width);
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
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_centering_mode(CenteringMode::JustInView);

        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window(2, Some(800)).unwrap();
        ws.insert_window(3, Some(800)).unwrap();

        let viewport = Rect::new(0, 0, 1920, 1080);
        let vp_width = viewport.width;

        ws.mark_minimized(1);
        ws.mark_minimized(2);
        ws.mark_minimized(3);
        ws.ensure_focused_visible_animated(vp_width);
        for _ in 0..100 {
            if !ws.tick_animation(16) {
                break;
            }
        }

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
        assert_eq!(p[0].rect.x, 10);

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
        assert_ne!(p[0].rect.x, p[1].rect.x);

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
        let xs: Vec<i32> = p.iter().map(|pl| pl.rect.x).collect();
        assert_ne!(xs[0], xs[1], "window 1 and 2 overlap at x={}", xs[0]);
        assert_ne!(xs[1], xs[2], "window 2 and 3 overlap at x={}", xs[1]);
        assert_ne!(xs[0], xs[2], "window 1 and 3 overlap at x={}", xs[0]);
    }

    #[test]
    fn test_insert_window_no_focus_preserves_focus() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        assert_eq!(ws.focused_window(), Some(3));

        ws.insert_window_no_focus(4, Some(400)).unwrap();
        assert_eq!(ws.focused_window(), Some(3));
        assert!(ws.contains_window(4));
        assert_eq!(ws.window_count(), 4);
    }

    #[test]
    fn test_insert_window_no_focus_into_empty_workspace() {
        let mut ws = Workspace::new();
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

    #[test]
    fn test_move_window_left_basic() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        assert_eq!(ws.focused_column_index(), 1);

        ws.move_window_left();

        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1, 2]);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_move_window_left_multi() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window_in_column(3, 1).unwrap();
        ws.test_set_focus_unchecked(1, 0);

        ws.move_window_left();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[1, 2]);
        assert_eq!(ws.columns()[1].windows(), &[3]);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_move_window_left_at_edge() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        assert_eq!(ws.focused_column_index(), 0);

        ws.move_window_left();

        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1]);
    }

    #[test]
    fn test_move_window_right_basic() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.focus_left();
        assert_eq!(ws.focused_column_index(), 0);

        ws.move_window_right();

        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[2, 1]);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_move_window_right_multi() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        ws.test_set_focus_unchecked(0, 0);

        ws.move_window_right();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[2]);
        assert_eq!(ws.columns()[1].windows(), &[3, 1]);
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_move_window_right_at_edge() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        assert_eq!(ws.focused_column_index(), 0);

        ws.move_window_right();

        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1]);
    }

    #[test]
    fn test_move_window_right_single_window_column() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        ws.focus_left();
        ws.focus_left();
        assert_eq!(ws.focused_column_index(), 0);

        ws.move_window_right();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[2, 1]);
        assert_eq!(ws.columns()[1].windows(), &[3]);
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_move_window_left_skips_minimized_column() {
        for stacked in [false, true] {
            let mut ws = Workspace::new();
            ws.insert_window(1, Some(400)).unwrap();
            ws.insert_window(2, Some(400)).unwrap();
            ws.insert_window(3, Some(400)).unwrap();
            if stacked {
                ws.insert_window_in_column(4, 2).unwrap();
            }
            ws.mark_minimized(2);
            ws.test_set_focus_unchecked(2, 0);

            ws.move_window_left();

            assert_eq!(ws.column_count(), if stacked { 3 } else { 2 });
            assert_eq!(ws.columns()[0].windows(), &[1, 3]);
            assert_eq!(ws.columns()[1].windows(), &[2]);
            if stacked {
                assert_eq!(ws.columns()[2].windows(), &[4]);
            }
            assert_eq!(ws.focused_column_index(), 0);
            assert_eq!(ws.focused_window(), Some(3));
            assert!(ws.is_minimized(2));
        }
    }

    #[test]
    fn test_move_window_right_skips_minimized_column() {
        for stacked in [false, true] {
            let mut ws = Workspace::new();
            ws.insert_window(1, Some(400)).unwrap();
            ws.insert_window(2, Some(400)).unwrap();
            ws.insert_window(3, Some(400)).unwrap();
            if stacked {
                ws.insert_window_in_column(4, 0).unwrap();
            }
            ws.mark_minimized(2);
            ws.test_set_focus_unchecked(0, 0);

            ws.move_window_right();

            assert_eq!(ws.column_count(), if stacked { 3 } else { 2 });
            let target = if stacked { 2 } else { 1 };
            if stacked {
                assert_eq!(ws.columns()[0].windows(), &[4]);
            }
            assert_eq!(ws.columns()[target - 1].windows(), &[2]);
            assert_eq!(ws.columns()[target].windows(), &[3, 1]);
            assert_eq!(ws.focused_column_index(), target);
            assert_eq!(ws.focused_window(), Some(1));
            assert!(ws.is_minimized(2));
        }
    }

    #[test]
    fn test_move_window_left_beyond_active_edge_unstacks_or_noops() {
        for stacked in [false, true] {
            let mut ws = Workspace::new();
            ws.insert_window(1, Some(400)).unwrap();
            ws.insert_window(2, Some(400)).unwrap();
            if stacked {
                ws.insert_window_in_column(3, 1).unwrap();
            }
            ws.mark_minimized(1);
            ws.test_set_focus_unchecked(1, 0);

            ws.move_window_left();

            assert_eq!(ws.column_count(), if stacked { 3 } else { 2 });
            assert_eq!(ws.columns()[0].windows(), &[1]);
            assert_eq!(ws.columns()[1].windows(), &[2]);
            if stacked {
                assert_eq!(ws.columns()[2].windows(), &[3]);
            }
            assert_eq!(ws.focused_column_index(), 1);
            assert_eq!(ws.focused_window(), Some(2));
            assert!(ws.is_minimized(1));
        }
    }

    #[test]
    fn test_move_window_right_beyond_active_edge_unstacks_or_noops() {
        for stacked in [false, true] {
            let mut ws = Workspace::new();
            ws.insert_window(1, Some(400)).unwrap();
            ws.insert_window(2, Some(400)).unwrap();
            if stacked {
                ws.insert_window_in_column(3, 0).unwrap();
            }
            ws.mark_minimized(2);
            ws.test_set_focus_unchecked(0, 0);

            ws.move_window_right();

            assert_eq!(ws.column_count(), if stacked { 3 } else { 2 });
            let target = if stacked { 1 } else { 0 };
            if stacked {
                assert_eq!(ws.columns()[0].windows(), &[3]);
            }
            assert_eq!(ws.columns()[target].windows(), &[1]);
            assert_eq!(ws.columns()[target + 1].windows(), &[2]);
            assert_eq!(ws.focused_column_index(), target);
            assert_eq!(ws.focused_window(), Some(1));
            assert!(ws.is_minimized(2));
        }
    }

    #[test]
    fn test_expel_to_left() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();
        ws.test_set_focus_unchecked(0, 1);

        ws.expel_to_left();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[2]);
        assert_eq!(ws.columns()[1].windows(), &[1, 3]);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_expel_to_right() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();
        ws.test_set_focus_unchecked(0, 1);

        ws.expel_to_right();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[1, 3]);
        assert_eq!(ws.columns()[1].windows(), &[2]);
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_expel_single_window_noop() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();

        ws.expel_to_left();
        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1]);

        ws.expel_to_right();
        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1]);
    }

    #[test]
    fn test_consume_from_right_merges_neighbor() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.test_set_focus_unchecked(0, 0);

        ws.consume_from_right();

        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1, 2]);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_consume_from_right_pulls_top_of_multi() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window_in_column(3, 1).unwrap();
        ws.test_set_focus_unchecked(0, 0);

        ws.consume_from_right();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[1, 2]);
        assert_eq!(ws.columns()[1].windows(), &[3]);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_consume_from_left_merges_neighbor() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.test_set_focus_unchecked(1, 0);

        ws.consume_from_left();

        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[2, 1]);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_consume_from_right_skips_minimized_column() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        ws.mark_minimized(2);
        ws.test_set_focus_unchecked(0, 0);

        ws.consume_from_right();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[1, 3]);
        assert_eq!(ws.columns()[1].windows(), &[2]);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(3));
        assert!(ws.is_minimized(2));
    }

    #[test]
    fn test_consume_from_right_skips_minimized_top_window() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window_in_column(3, 1).unwrap();
        ws.mark_minimized(2);
        ws.test_set_focus_unchecked(0, 0);

        ws.consume_from_right();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[1, 3]);
        assert_eq!(ws.columns()[1].windows(), &[2]);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window(), Some(3));
        assert!(ws.is_minimized(2));
    }

    #[test]
    fn test_consume_from_left_skips_minimized_column() {
        let mut ws = Workspace::new();
        ws.insert_window(3, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window(1, Some(400)).unwrap();
        ws.mark_minimized(2);
        ws.test_set_focus_unchecked(2, 0);

        ws.consume_from_left();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[2]);
        assert_eq!(ws.columns()[1].windows(), &[1, 3]);
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(3));
        assert!(ws.is_minimized(2));
    }

    #[test]
    fn test_consume_from_left_skips_minimized_top_window() {
        let mut ws = Workspace::new();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();
        ws.insert_window(1, Some(400)).unwrap();
        ws.mark_minimized(2);
        ws.test_set_focus_unchecked(1, 0);

        ws.consume_from_left();

        assert_eq!(ws.column_count(), 2);
        assert_eq!(ws.columns()[0].windows(), &[2]);
        assert_eq!(ws.columns()[1].windows(), &[1, 3]);
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(3));
        assert!(ws.is_minimized(2));
    }

    #[test]
    fn test_consume_noop_without_neighbor() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.test_set_focus_unchecked(0, 0);

        ws.consume_from_right();
        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1]);

        ws.consume_from_left();
        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1]);
    }

    #[test]
    fn test_consume_into_tabbed_column_activates_consumed() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window(3, Some(400)).unwrap();
        ws.set_focus(0, 0).unwrap();
        ws.toggle_focused_column_tabbed_mode();

        ws.consume_from_right();

        assert_eq!(ws.column_count(), 1);
        assert!(ws.columns()[0].is_tabbed());
        assert_eq!(ws.columns()[0].windows(), &[1, 2, 3]);
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(2));
        assert_eq!(ws.focused_window(), Some(3));
    }

    #[test]
    fn test_move_window_up() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();
        ws.test_set_focus_unchecked(0, 2);

        ws.move_window_up_in_column();

        assert_eq!(ws.columns()[0].windows(), &[1, 3, 2]);
        assert_eq!(ws.focused_window_index_in_column(), 1);
        assert_eq!(ws.focused_window(), Some(3));
    }

    #[test]
    fn test_move_window_down() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();
        ws.test_set_focus_unchecked(0, 0);

        ws.move_window_down_in_column();

        assert_eq!(ws.columns()[0].windows(), &[2, 1, 3]);
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
    fn test_movement_on_empty_tiled_strip_is_noop() {
        let empty = Workspace::new();
        let mut emptied = Workspace::new();
        emptied.insert_window(1, Some(400)).unwrap();
        emptied.remove_window(1).unwrap();
        let mut floating_only = Workspace::new();
        floating_only
            .add_floating(2, Rect::new(10, 20, 400, 300))
            .unwrap();

        type Movement = fn(&mut Workspace);
        let movements: [(&str, Movement); 12] = [
            ("column left", Workspace::move_column_left),
            ("column right", Workspace::move_column_right),
            ("column start", Workspace::move_column_to_start),
            ("column end", Workspace::move_column_to_end),
            ("window left", Workspace::move_window_left),
            ("window right", Workspace::move_window_right),
            ("window up", Workspace::move_window_up_in_column),
            ("window down", Workspace::move_window_down_in_column),
            ("expel left", Workspace::expel_to_left),
            ("expel right", Workspace::expel_to_right),
            ("consume left", Workspace::consume_from_left),
            ("consume right", Workspace::consume_from_right),
        ];
        for (state, workspace) in [
            ("empty", empty),
            ("last window removed", emptied),
            ("floating only", floating_only),
        ] {
            assert_eq!(workspace.focused_window(), None);
            let before = serde_json::to_value(&workspace).unwrap();
            for (name, movement) in movements {
                let mut ws = workspace.clone();
                // Repeated input must also leave membership, geometry and focus unchanged.
                for _ in 0..2 {
                    movement(&mut ws);
                    assert_eq!(
                        serde_json::to_value(&ws).unwrap(),
                        before,
                        "{name} changed {state} workspace"
                    );
                }
            }
        }
    }

    #[test]
    fn test_vertical_movement_in_tabbed_column_preserves_window_focus() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 1).unwrap();
        let focused = ws.focused_window();

        ws.move_window_down_in_column();
        assert_eq!(ws.columns()[0].windows(), &[1, 3, 2]);
        assert_eq!(ws.focused_window(), focused);
        assert_eq!(ws.focused_window_index_in_column(), 2);
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(2));

        let bottom = serde_json::to_value(&ws).unwrap();
        ws.move_window_down_in_column();
        assert_eq!(serde_json::to_value(&ws).unwrap(), bottom);

        ws.move_window_up_in_column();
        ws.move_window_up_in_column();
        assert_eq!(ws.columns()[0].windows(), &[2, 1, 3]);
        assert_eq!(ws.focused_window(), focused);
        assert_eq!(ws.focused_window_index_in_column(), 0);
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(0));

        let top = serde_json::to_value(&ws).unwrap();
        ws.move_window_up_in_column();
        assert_eq!(serde_json::to_value(&ws).unwrap(), top);
    }

    #[test]
    fn test_move_window_preserves_minimized() {
        let mut ws = Workspace::new();
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.mark_minimized(2);
        assert_eq!(ws.focused_column_index(), 1);

        ws.move_window_left();

        assert!(ws.is_minimized(2));
        assert_eq!(ws.column_count(), 1);
        assert_eq!(ws.columns()[0].windows(), &[1, 2]);
    }

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
        col.set_height_weight(0, 0.667);
        let w0 = col.height_weights()[0];
        let w1 = col.height_weights()[1];
        assert!((w0 - 0.667).abs() < 0.01);

        col.swap_windows(0, 1);
        assert!((col.height_weights()[0] - w1).abs() < 1e-9);
        assert!((col.height_weights()[1] - w0).abs() < 1e-9);
    }

    #[test]
    fn test_placements_with_height_weights() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();

        ws.columns_mut()[0].set_height_weight(0, 0.667);

        let viewport = Rect::new(0, 0, 800, 900);
        let placements = ws.compute_placements(viewport);

        assert_eq!(placements.len(), 2);
        // Window 1 (2/3 of 900 = 600)
        assert!((placements[0].rect.height - 600).abs() <= 1);
        // Window 2 (1/3 of 900 = 300)
        assert!((placements[1].rect.height - 300).abs() <= 1);
        assert_eq!(
            placements[0].rect.height + placements[1].rect.height,
            viewport.height
        );
    }

    #[test]
    fn test_cycle_width_wrap_presets() {
        for (current, expected) in [
            (750, 250),
            (900, 250),
            (400, 500),
            (494, 500),
            (495, 750),
            (496, 750),
            (746, 250),
        ] {
            let mut ws = Workspace::with_gaps(0, 0);
            ws.insert_window(1, Some(current)).unwrap();
            ws.cycle_width(&[0.75, 0.25, 0.5], 1000);
            assert_eq!(ws.columns()[0].width(), expected, "current={current}");
        }
    }

    #[test]
    fn test_cycle_width_wrap_uses_effective_width_and_resets_maximize() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(250)).unwrap();
        ws.set_window_min_width(1, 600);
        ws.cycle_width(&[0.25, 0.5, 0.75], 1000);
        assert_eq!(ws.columns()[0].width(), 750);
        ws.cycle_width(&[0.25, 0.5, 0.75], 1000);
        assert_eq!(ws.columns()[0].width(), 250);
        assert_eq!(ws.effective_column_width(&ws.columns()[0]), 600);
        assert!(ws.toggle_maximize_column(1000));
        ws.cycle_width(&[0.25, 0.5, 0.75], 1000);
        assert_eq!(ws.columns()[0].width(), 250);
        assert!(ws.toggle_maximize_column(1000));
    }

    #[test]
    fn test_cycle_height_wrap_presets() {
        for (current, expected) in [
            (0.75, 0.25),
            (0.9, 0.25),
            (0.4, 0.5),
            (0.494, 0.5),
            (0.495, 0.75),
            (0.496, 0.75),
            (0.746, 0.25),
        ] {
            let mut ws = Workspace::with_gaps(0, 0);
            ws.insert_window(1, Some(800)).unwrap();
            ws.insert_window_in_column(2, 0).unwrap();
            ws.set_focus(0, 0).unwrap();
            ws.columns_mut()[0].set_height_weight(0, current);
            ws.cycle_height(&[0.75, 0.25, 0.5]);
            let weights = ws.columns()[0].height_weights();
            assert!((weights[0] - expected).abs() < 1e-9, "current={current}");
            assert!((weights[1] - (1.0 - expected)).abs() < 1e-9);
        }
    }

    #[test]
    fn test_cycle_height_wrap_progresses_after_sibling_cap() {
        let cases: &[(u64, &[f64], &[f64])] = &[
            (
                8,
                &[0.333, 0.5, 0.667],
                &[0.333, 0.5, 0.65, 0.333, 0.5, 0.65, 0.333],
            ),
            (
                4,
                &[0.333, 0.5, 0.667, 0.9],
                &[0.333, 0.5, 0.667, 0.85, 0.333, 0.5, 0.667, 0.85, 0.333],
            ),
            (
                2,
                &[0.25, 0.5, 1.0],
                &[0.95, 0.25, 0.5, 0.95, 0.25, 0.5, 0.95, 0.25],
            ),
        ];
        let mut failures = Vec::new();
        for &(windows, presets, expected_weights) in cases {
            let mut ws = Workspace::with_gaps(0, 0);
            ws.insert_window(1, Some(800)).unwrap();
            for window in 2..=windows {
                ws.insert_window_in_column(window, 0).unwrap();
            }
            ws.set_focus(0, 0).unwrap();
            for (step, &expected) in expected_weights.iter().enumerate() {
                ws.cycle_height(presets);
                let actual = ws.columns()[0].height_weights()[0];
                if (actual - expected).abs() >= 1e-9 {
                    failures.push(format!(
                        "windows={windows}, step={step}: expected {expected}, got {actual}"
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn test_cycle_height_many_windows_keeps_positive_normalized_weights() {
        for windows in [20, 21, 22, 23, 100] {
            let mut ws = Workspace::with_gaps(0, 0);
            ws.insert_window(1, Some(800)).unwrap();
            for window in 2..=windows {
                ws.insert_window_in_column(window, 0).unwrap();
            }
            ws.set_focus(0, 0).unwrap();

            for _ in 0..3 {
                ws.cycle_height(&[0.25, 0.5, 0.75]);
                let weights = ws.columns()[0].height_weights();
                assert_eq!(weights.len(), windows as usize);
                assert!(weights
                    .iter()
                    .all(|&weight| weight.is_finite() && weight > 0.0));
                assert!((weights.iter().sum::<f64>() - 1.0).abs() < 1e-9);
                for &weight in weights {
                    assert!((weight - 1.0 / windows as f64).abs() < 1e-9);
                }
            }
        }
    }

    #[test]
    fn test_cycle_height_up_still_stops_at_sibling_cap() {
        let cases: &[(u64, &[f64], usize, f64)] = &[
            (8, &[0.333, 0.5, 0.667], 3, 0.65),
            (4, &[0.333, 0.5, 0.667, 0.9], 4, 0.85),
            (2, &[0.25, 0.5, 1.0], 1, 0.95),
        ];
        for &(windows, presets, steps, cap) in cases {
            let mut ws = Workspace::with_gaps(0, 0);
            ws.insert_window(1, Some(800)).unwrap();
            for window in 2..=windows {
                ws.insert_window_in_column(window, 0).unwrap();
            }
            ws.set_focus(0, 0).unwrap();
            for _ in 0..steps {
                ws.cycle_height_up(presets);
            }
            let capped = ws.columns()[0].height_weights().to_vec();
            assert!((capped[0] - cap).abs() < 1e-9, "windows={windows}");
            for _ in 0..3 {
                ws.cycle_height_up(presets);
                assert_eq!(
                    ws.columns()[0].height_weights(),
                    capped,
                    "windows={windows}"
                );
            }
        }
    }

    #[test]
    fn test_cycle_height_wrap_noop_cases() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(800)).unwrap();
        ws.cycle_height(&[0.25, 0.5, 0.75]);
        assert_eq!(ws.columns()[0].height_weights(), &[1.0]);
        ws.insert_window_in_column(2, 0).unwrap();
        ws.set_focus(0, 0).unwrap();
        ws.columns_mut()[0].set_height_weight(0, 0.75);
        ws.toggle_focused_column_tabbed_mode();
        let before = ws.columns()[0].height_weights().to_vec();
        ws.cycle_height(&[0.25, 0.5, 0.75]);
        assert_eq!(ws.columns()[0].height_weights(), before);
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
        assert_eq!(ws.columns()[0].width(), w);
    }

    #[test]
    fn test_cycle_height_single_window_noop() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(800)).unwrap();
        let presets = vec![0.333, 0.5, 0.667];

        ws.cycle_height_up(&presets);
        assert_eq!(ws.columns()[0].height_weights(), &[1.0]);
    }

    #[test]
    fn test_equalize_heights() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.set_focus(0, 0).unwrap();

        let presets = vec![0.333, 0.5, 0.667];
        ws.cycle_height_up(&presets);
        assert!((ws.columns()[0].height_weights()[0] - 0.667).abs() < 1e-9);

        ws.equalize_focused_column_heights();
        assert!((ws.columns()[0].height_weights()[0] - 0.5).abs() < 1e-9);
        assert!((ws.columns()[0].height_weights()[1] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_height_weights_backward_compat() {
        let col: Column = serde_json::from_str(r#"{"width":800,"windows":[1,2]}"#).unwrap();
        assert!(col.height_weights.is_empty());
        // Compute placements should fall back to equal distribution
        let mut ws = Workspace::with_gaps(0, 0);
        ws.rows[0].columns.push(col);
        ws.rows[0].focused_column = 0;
        ws.rows[0].focused_window_in_column = 0;
        let viewport = Rect::new(0, 0, 800, 1000);
        let placements = ws.compute_placements(viewport);
        assert_eq!(placements.len(), 2);
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

        ws.columns_mut()[1].set_height_weight(0, 0.667);

        ws.set_focus(1, 0).unwrap();
        ws.move_window_left();

        let weights = ws.columns()[0].height_weights();
        assert_eq!(weights.len(), 2);
        assert!((weights[0] - 0.5).abs() < 1e-9);
        assert!((weights[1] - 0.5).abs() < 1e-9);
    }

    fn ws_3_tabs() -> Workspace {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();
        ws.set_focus(0, 0).unwrap();
        ws
    }

    #[test]
    fn test_column_default_mode_is_vertical() {
        let col = Column::new(1, 800);
        assert!(matches!(col.mode(), ColumnMode::Vertical));
        assert!(!col.is_tabbed());
        assert_eq!(col.active_tab_idx(), None);
    }

    #[test]
    fn test_set_tabbed_clamps_active_idx() {
        let mut col = Column::new(1, 800);
        col.add_window(2);
        col.add_window(3);
        col.set_tabbed(99);
        assert_eq!(col.active_tab_idx(), Some(2));
    }

    #[test]
    fn test_set_tabbed_noop_on_empty_column() {
        let mut col = Column::empty(800);
        col.set_tabbed(0);
        assert!(matches!(col.mode(), ColumnMode::Vertical));
    }

    #[test]
    fn test_toggle_focused_column_tabbed_mode_seeds_active() {
        let mut ws = ws_3_tabs();
        ws.set_focus(0, 1).unwrap();
        ws.toggle_focused_column_tabbed_mode();
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(1));
        ws.toggle_focused_column_tabbed_mode();
        assert!(matches!(ws.columns()[0].mode(), ColumnMode::Vertical));
    }

    #[test]
    fn test_toggle_tabbed_noop_on_single_window_column() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(800)).unwrap();
        ws.toggle_focused_column_tabbed_mode();
        assert!(matches!(ws.columns()[0].mode(), ColumnMode::Vertical));
    }

    #[test]
    fn test_set_active_tab_updates_focus_when_focused() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 2).unwrap();
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(2));
        assert_eq!(ws.focused_window_in_column(), 2);
    }

    #[test]
    fn test_set_active_tab_no_focus_change_when_unfocused() {
        let mut ws = ws_3_tabs();
        ws.insert_window(99, Some(800)).unwrap();
        ws.set_focus(0, 0).unwrap();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_focus(1, 0).unwrap();
        ws.set_active_tab(0, 2).unwrap();
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(2));
        assert_eq!(ws.focused_column_index(), 1);
    }

    #[test]
    fn test_set_active_tab_errors_on_vertical_column() {
        let mut ws = ws_3_tabs();
        assert!(ws.set_active_tab(0, 1).is_err());
    }

    #[test]
    fn test_set_active_tab_errors_on_out_of_range() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        assert!(ws.set_active_tab(0, 99).is_err());
        assert!(ws.set_active_tab(99, 0).is_err());
    }

    #[test]
    fn test_focus_up_cycles_tabs_with_wrap() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.focus_up();
        assert_eq!(ws.focused_window_in_column(), 2);
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(2));
        ws.focus_up();
        assert_eq!(ws.focused_window_in_column(), 1);
    }

    #[test]
    fn test_focus_down_cycles_tabs_with_wrap() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.focus_down();
        assert_eq!(ws.focused_window_in_column(), 1);
        ws.focus_down();
        assert_eq!(ws.focused_window_in_column(), 2);
        ws.focus_down();
        assert_eq!(ws.focused_window_in_column(), 0);
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(0));
    }

    #[test]
    fn test_focus_up_in_vertical_no_wrap() {
        let mut ws = ws_3_tabs();
        ws.set_focus(0, 0).unwrap();
        ws.focus_up();
        assert_eq!(ws.focused_window_in_column(), 0);
    }

    #[test]
    fn test_focus_left_into_tabbed_lands_on_active() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.insert_window_in_column(3, 0).unwrap();
        ws.set_focus(0, 2).unwrap();
        ws.toggle_focused_column_tabbed_mode();
        ws.insert_window(99, Some(800)).unwrap();
        ws.set_focus(1, 0).unwrap();
        ws.focus_left();
        assert_eq!(ws.focused_column_index(), 0);
        // Should land on the active tab (2), not whatever index was carried.
        assert_eq!(ws.focused_window_in_column(), 2);
    }

    #[test]
    fn test_layout_only_active_tab_visible() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 1).unwrap();
        let viewport = Rect::new(0, 0, 800, 1000);
        let placements = ws.compute_placements(viewport);
        // Three placements: active (visible) + 2 off-screen.
        assert_eq!(placements.len(), 3);
        let visible: Vec<_> = placements
            .iter()
            .filter(|p| matches!(p.visibility, Visibility::Visible))
            .collect();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].window_id, 2);
        assert_eq!(visible[0].rect.height, 1000);
    }

    #[test]
    fn test_layout_tabbed_active_minimized_falls_back() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 1).unwrap();
        ws.mark_minimized(2);
        let viewport = Rect::new(0, 0, 800, 1000);
        let placements = ws.compute_placements(viewport);
        // Visible should fall back to first non-minimized tab (window 1).
        let visible: Vec<_> = placements
            .iter()
            .filter(|p| matches!(p.visibility, Visibility::Visible))
            .collect();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].window_id, 1);
    }

    #[test]
    fn test_column_removal_synchronizes_replacement_tab() {
        for remove_whole_column in [false, true] {
            for remove_left in [false, true] {
                let mut ws = ws_3_tabs();
                ws.toggle_focused_column_tabbed_mode();
                ws.set_active_tab(0, 2).unwrap();
                ws.insert_window(99, Some(400)).unwrap();
                if remove_left {
                    ws.reorder_column(1, 0);
                }
                assert_eq!(ws.focused_window(), Some(99));

                if remove_whole_column {
                    let removed = ws.remove_column(ws.focused_column_index()).unwrap();
                    assert_eq!(removed.windows(), &[99]);
                } else {
                    ws.remove_window(99).unwrap();
                }

                // Replacement focus and the visible tab must identify the same window.
                assert_eq!(ws.column_count(), 1);
                assert_eq!(ws.columns()[0].windows(), &[1, 2, 3]);
                assert_eq!(ws.focused_column_index(), 0);
                assert_eq!(ws.focused_window_index_in_column(), 0);
                assert_eq!(ws.focused_window(), Some(1));
                assert_eq!(ws.columns()[0].active_tab_idx(), Some(0));
                let visible: Vec<_> = ws
                    .compute_placements(Rect::new(0, 0, 1920, 1080))
                    .into_iter()
                    .filter(|p| p.visibility == Visibility::Visible)
                    .map(|p| p.window_id)
                    .collect();
                assert_eq!(visible, vec![1]);
            }
        }
    }

    #[test]
    fn test_column_removal_clamps_replacement_tab_focus() {
        let mut ws = ws_3_tabs();
        ws.remove_window(3).unwrap();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 0).unwrap();
        ws.insert_window(99, Some(400)).unwrap();
        ws.insert_window_in_column(100, 1).unwrap();
        ws.insert_window_in_column(101, 1).unwrap();
        ws.set_focus(1, 2).unwrap();

        let removed = ws.remove_column(1).unwrap();
        assert_eq!(removed.windows(), &[99, 100, 101]);
        assert_eq!(ws.columns()[0].windows(), &[1, 2]);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window_index_in_column(), 1);
        assert_eq!(ws.focused_window(), Some(2));
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(1));
    }

    #[test]
    fn test_nonfocused_column_removal_preserves_tab_focus() {
        for remove_whole_column in [false, true] {
            for remove_left in [false, true] {
                let mut ws = ws_3_tabs();
                ws.toggle_focused_column_tabbed_mode();
                ws.set_active_tab(0, 2).unwrap();
                ws.insert_window(99, Some(400)).unwrap();
                if remove_left {
                    ws.reorder_column(1, 0);
                }
                let removed_index = ws.focused_column_index();
                let tabbed_index = usize::from(remove_left);
                ws.set_focus(tabbed_index, 2).unwrap();

                if remove_whole_column {
                    let removed = ws.remove_column(removed_index).unwrap();
                    assert_eq!(removed.windows(), &[99]);
                } else {
                    ws.remove_window(99).unwrap();
                }

                assert_eq!(ws.column_count(), 1);
                assert_eq!(ws.columns()[0].windows(), &[1, 2, 3]);
                assert_eq!(ws.focused_column_index(), 0);
                assert_eq!(ws.focused_window_index_in_column(), 2);
                assert_eq!(ws.focused_window(), Some(3));
                assert_eq!(ws.columns()[0].active_tab_idx(), Some(2));
            }
        }
    }

    #[test]
    fn test_remove_active_tab_advances_active_idx() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 1).unwrap();
        ws.remove_window(2).unwrap();
        // Removing the active tab selects its successor in the same slot.
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(1));
        assert_eq!(ws.columns()[0].windows()[1], 3);
    }

    #[test]
    fn test_remove_before_active_decrements_active_idx() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 2).unwrap();
        ws.remove_window(1).unwrap();
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(1));
        // The window at the new active idx is still window 3 (originally idx 2).
        assert_eq!(ws.columns()[0].windows()[1], 3);
    }

    #[test]
    fn test_remove_after_active_keeps_active_idx() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 0).unwrap();
        ws.remove_window(3).unwrap();
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(0));
        assert_eq!(ws.columns()[0].windows()[0], 1);
    }

    #[test]
    fn test_remove_drops_to_one_auto_reverts_to_vertical() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 1).unwrap();
        ws.remove_window(1).unwrap();
        ws.remove_window(2).unwrap();
        assert!(matches!(ws.columns()[0].mode(), ColumnMode::Vertical));
    }

    #[test]
    fn test_insert_at_or_before_active_shifts_active() {
        for insert_index in [0, 1] {
            let mut ws = ws_3_tabs();
            ws.toggle_focused_column_tabbed_mode();
            ws.set_active_tab(0, 1).unwrap();
            ws.insert_window_in_column_at(99, 0, insert_index).unwrap();
            assert_eq!(ws.columns()[0].active_tab_idx(), Some(2));
            assert_eq!(ws.columns()[0].windows()[2], 2);
        }
    }

    #[test]
    fn test_insert_after_active_keeps_active_idx() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 0).unwrap();
        ws.insert_window_in_column_at(99, 0, 3).unwrap();
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(0));
    }

    #[test]
    fn test_swap_around_active_tracks_window() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 1).unwrap();
        ws.columns_mut()[0].swap_windows(0, 1);
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(0));
        // Window at the new active idx is the original "active" window (was at idx 1).
        assert_eq!(ws.columns()[0].windows()[0], 2);
    }

    #[test]
    fn test_move_window_up_in_tabbed_column_tracks_active() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 1).unwrap();
        ws.move_window_up_in_column();
        assert_eq!(ws.focused_window_in_column(), 0);
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(0));
    }

    #[test]
    fn test_move_window_right_and_left_into_tabbed_keeps_focus_on_moved() {
        for move_right in [true, false] {
            let mut ws = Workspace::with_gaps(0, 0);
            ws.insert_window(1, Some(800)).unwrap();
            ws.insert_window(2, Some(800)).unwrap();
            let receiver = usize::from(move_right);
            let source = 1 - receiver;
            ws.insert_window_in_column(3, receiver).unwrap();
            ws.set_focus(receiver, 0).unwrap();
            ws.toggle_focused_column_tabbed_mode();
            let moved = ws.columns()[source].windows()[0];
            ws.set_focus(source, 0).unwrap();

            if move_right {
                ws.move_window_right();
            } else {
                ws.move_window_left();
            }

            assert_eq!(ws.columns().len(), 1);
            assert_eq!(ws.columns()[0].active_tab_idx(), Some(2));
            assert_eq!(ws.focused_window_in_column(), 2);
            assert_eq!(ws.focused_window(), Some(moved));
            assert_eq!(ws.columns()[0].windows()[2], moved);
        }
    }

    #[test]
    fn test_expel_active_tab_creates_vertical_column() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 1).unwrap();
        ws.set_focus(0, 1).unwrap();
        ws.expel_to_right();
        // The successor occupies the expelled tab's unchanged active index.
        assert!(ws.columns()[0].is_tabbed());
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(1));
        assert_eq!(ws.columns()[0].windows(), &[1, 3]);
        assert!(matches!(ws.columns()[1].mode(), ColumnMode::Vertical));
        assert_eq!(ws.columns()[1].windows(), &[2]);
    }

    #[test]
    fn test_expel_drops_to_one_auto_reverts_to_vertical() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.set_focus(0, 0).unwrap();
        ws.toggle_focused_column_tabbed_mode();
        assert!(ws.columns()[0].is_tabbed());
        ws.set_focus(0, 1).unwrap();
        ws.expel_to_right();
        assert!(matches!(ws.columns()[0].mode(), ColumnMode::Vertical));
    }

    #[test]
    fn test_workspace_json_roundtrip_with_tabbed_column() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 1).unwrap();
        let json = serde_json::to_string(&ws).unwrap();
        let restored: Workspace = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.columns()[0].active_tab_idx(),
            Some(1),
            "active_idx should round-trip through JSON"
        );
        assert!(restored.columns()[0].is_tabbed());
        assert_eq!(restored.focused_window_in_column(), 1);
    }

    #[test]
    fn test_workspace_json_legacy_payload_defaults_to_vertical() {
        let legacy = r#"{
            "columns": [{"width": 800, "windows": [1, 2, 3], "height_weights": [0.33, 0.33, 0.34]}],
            "focused_column": 0,
            "focused_window_in_column": 0,
            "scroll_offset": 0.0,
            "gap": 0,
            "outer_gap_left": 0,
            "outer_gap_right": 0,
            "outer_gap_top": 0,
            "outer_gap_bottom": 0,
            "default_column_width": 800,
            "centering_mode": "Center"
        }"#;
        let ws: Workspace = serde_json::from_str(legacy).unwrap();
        assert!(matches!(ws.columns()[0].mode(), ColumnMode::Vertical));
        assert_eq!(ws.columns()[0].width(), 800);
        assert_eq!(ws.effective_column_width(&ws.columns()[0]), 800);
    }

    #[test]
    fn test_cycle_height_noop_on_tabbed_column() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        let before = ws.columns()[0].height_weights().to_vec();
        ws.cycle_height_up(&[0.333, 0.5, 0.667]);
        let after = ws.columns()[0].height_weights().to_vec();
        assert_eq!(before, after, "cycle_height should be no-op on Tabbed");
    }

    #[test]
    fn test_equalize_heights_noop_on_tabbed_column() {
        let mut ws = ws_3_tabs();
        // Set unequal weights first (in Vertical mode).
        ws.columns_mut()[0].set_height_weight(0, 0.7);
        let before = ws.columns()[0].height_weights().to_vec();
        ws.toggle_focused_column_tabbed_mode();
        ws.equalize_focused_column_heights();
        let after = ws.columns()[0].height_weights().to_vec();
        assert_eq!(before, after, "equalize should be no-op on Tabbed");
    }

    #[test]
    fn test_snap_height_noop_on_tabbed_column() {
        let mut ws = ws_3_tabs();
        let before = ws.columns()[0].height_weights().to_vec();
        ws.toggle_focused_column_tabbed_mode();
        ws.snap_window_height_to_preset(0, 1, 600, &[0.333, 0.5, 0.667], 1000);
        let after = ws.columns()[0].height_weights().to_vec();
        assert_eq!(before, after, "snap_height should be no-op on Tabbed");
    }

    #[test]
    fn test_focus_down_with_minimized_tabs_skips() {
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_active_tab(0, 0).unwrap();
        ws.mark_minimized(2);
        ws.focus_down();
        assert_eq!(ws.focused_window_in_column(), 2);
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(2));
        assert_eq!(ws.focused_window(), Some(3));

        ws.focus_up();
        assert_eq!(ws.focused_window_in_column(), 0);
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(0));
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_active_idx_invariant_with_focus() {
        // A focused tabbed column keeps its active tab synchronized with window focus.
        let mut ws = ws_3_tabs();
        ws.toggle_focused_column_tabbed_mode();
        ws.set_focus(0, 1).unwrap();
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(1));
        ws.set_focus(0, 2).unwrap();
        assert_eq!(ws.columns()[0].active_tab_idx(), Some(2));
    }

    #[test]
    fn test_insert_in_tabbed_column_preserves_active_idx_without_focus() {
        // Appending without a follow-up focus call preserves the active tab index.
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window(2, Some(800)).unwrap();
        ws.insert_window_in_column(3, 1).unwrap();
        ws.set_focus(1, 1).unwrap();
        ws.toggle_focused_column_tabbed_mode();
        assert_eq!(ws.columns()[1].active_tab_idx(), Some(1));
        ws.insert_window_in_column_at(99, 1, 2).unwrap();
        assert_eq!(ws.columns()[1].active_tab_idx(), Some(1));
    }

    // ========================================================================
    // Multi-Row Scrolling Engine Tests (Phase 1)
    // ========================================================================

    #[test]
    fn test_multi_row_rect_partitioning() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_row_gap(10);
        let viewport = Rect::new(0, 0, 1920, 1080);

        // 1 row: full content height = 1080 - 10 (top) - 10 (bottom) = 1060
        let rects_1 = ws.row_rects(viewport);
        assert_eq!(rects_1.len(), 1);
        assert_eq!(rects_1[0], Rect::new(0, 10, 1920, 1060));

        // 2 equal rows: content height = 1080 - 10 - 10 - 10 (row_gap) = 1050
        // Each row gets 525 px.
        ws.set_row_layout(&[1.0, 1.0]);
        let rects_2 = ws.row_rects(viewport);
        assert_eq!(rects_2.len(), 2);
        assert_eq!(rects_2[0], Rect::new(0, 10, 1920, 525));
        assert_eq!(rects_2[1], Rect::new(0, 10 + 525 + 10, 1920, 525));
        // Verify total height coverage
        assert_eq!(10 + rects_2[0].height + 10 + rects_2[1].height + 10, 1080);

        // 3 rows with fractional shares [1.0, 2.0, 1.0]
        // Content height = 1080 - 10 - 10 - 2 * 10 = 1040
        // Total shares = 4.0. Row 0: 260, Row 1: 520, Row 2: 260.
        ws.set_row_layout(&[1.0, 2.0, 1.0]);
        let rects_3 = ws.row_rects(viewport);
        assert_eq!(rects_3.len(), 3);
        assert_eq!(rects_3[0].height, 260);
        assert_eq!(rects_3[1].height, 520);
        assert_eq!(rects_3[2].height, 260);
    }

    #[test]
    fn test_multi_row_independent_scrolling() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_row_layout(&[1.0, 1.0]);

        // Insert in row 0
        ws.focus_row(0);
        ws.insert_window(1, Some(1500)).unwrap();
        ws.insert_window(2, Some(1500)).unwrap();

        // Insert in row 1
        ws.focus_row(1);
        ws.insert_window(3, Some(1500)).unwrap();
        ws.insert_window(4, Some(1500)).unwrap();

        // Scroll row 0
        ws.focus_row(0);
        ws.scroll_by(300.0, 1920);
        assert_eq!(ws.rows()[0].scroll_offset(), 300.0);
        assert_eq!(ws.rows()[1].scroll_offset(), 0.0);

        // Scroll row 1
        ws.focus_row(1);
        ws.scroll_by(450.0, 1920);
        assert_eq!(ws.rows()[0].scroll_offset(), 300.0);
        assert_eq!(ws.rows()[1].scroll_offset(), 450.0);

        // Scroll row 0 targeting row index directly
        ws.scroll_row_by(0, 50.0, 1920);
        assert_eq!(ws.rows()[0].scroll_offset(), 350.0);
        assert_eq!(ws.rows()[1].scroll_offset(), 450.0);
    }

    #[test]
    fn test_multi_row_placements_tag_row_index() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_row_layout(&[1.0, 1.0]);
        let viewport = Rect::new(0, 0, 1920, 1080);

        ws.focus_row(0);
        ws.insert_window(1, Some(800)).unwrap();

        ws.focus_row(1);
        ws.insert_window(2, Some(800)).unwrap();

        let placements = ws.compute_placements(viewport);
        assert_eq!(placements.len(), 2);

        let p1 = placements.iter().find(|p| p.window_id == 1).unwrap();
        let p2 = placements.iter().find(|p| p.window_id == 2).unwrap();

        assert_eq!(p1.row_index, 0);
        assert_eq!(p2.row_index, 1);

        let row_rects = ws.row_rects(viewport);
        assert!(p1.rect.y >= row_rects[0].y);
        assert!(p1.rect.bottom() <= row_rects[0].bottom());
        assert!(p2.rect.y >= row_rects[1].y);
        assert!(p2.rect.bottom() <= row_rects[1].bottom());
    }

    #[test]
    fn test_multi_row_focus_navigation_and_empty_row() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_row_layout(&[1.0, 1.0, 1.0]);

        ws.focus_row(0);
        ws.insert_window(1, Some(800)).unwrap();
        ws.focus_row(2);
        ws.insert_window(3, Some(800)).unwrap();

        // Row 1 is empty. D6: empty rows can be focused.
        assert_eq!(ws.window_count_for_row(1), 0);
        ws.focus_row(1);
        assert_eq!(ws.focused_row(), 1);

        // Inserting into empty focused row puts the new window there
        ws.insert_window(2, Some(800)).unwrap();
        assert_eq!(ws.window_count_for_row(1), 1);
        assert_eq!(ws.find_window_location_rc(2), Some((1, 0, 0)));

        // Focus row navigation
        ws.focus_row(0);
        ws.focus_row_down();
        assert_eq!(ws.focused_row(), 1);
        assert_eq!(ws.focused_window(), Some(2));

        ws.focus_row_down();
        assert_eq!(ws.focused_row(), 2);
        assert_eq!(ws.focused_window(), Some(3));

        ws.focus_row_up();
        assert_eq!(ws.focused_row(), 1);
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_multi_row_directional_focus_crossing() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_row_layout(&[1.0, 1.0]);

        ws.focus_row(0);
        ws.insert_window(1, Some(800)).unwrap();

        ws.focus_row(1);
        ws.insert_window(2, Some(800)).unwrap();

        ws.focus_row(0);
        assert_eq!(ws.focused_window(), Some(1));

        // Moving down at bottom of row 0 moves focus to row 1 (D2)
        ws.focus_down();
        assert_eq!(ws.focused_row(), 1);
        assert_eq!(ws.focused_window(), Some(2));

        // Moving up at top of row 1 moves focus to row 0 (D2)
        ws.focus_up();
        assert_eq!(ws.focused_row(), 0);
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_multi_row_move_window_to_row() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_row_layout(&[1.0, 1.0]);

        ws.focus_row(0);
        ws.insert_window(1, Some(800)).unwrap();
        ws.insert_window(2, Some(800)).unwrap();

        assert_eq!(ws.window_count_for_row(0), 2);
        assert_eq!(ws.window_count_for_row(1), 0);

        // Move window 1 to row 1
        ws.move_window_to_row(1, 1).unwrap();
        assert_eq!(ws.window_count_for_row(0), 1);
        assert_eq!(ws.window_count_for_row(1), 1);
        assert_eq!(ws.focused_row(), 1);
        assert_eq!(ws.focused_window(), Some(1));

        // Move window 1 back to row 0
        ws.move_window_to_row_up().unwrap();
        assert_eq!(ws.window_count_for_row(0), 2);
        assert_eq!(ws.window_count_for_row(1), 0);
        assert_eq!(ws.focused_row(), 0);
    }

    #[test]
    fn test_multi_row_reconciliation_add_and_remove() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_row_layout(&[1.0, 1.0, 1.0]);

        ws.focus_row(0);
        ws.insert_window(1, Some(800)).unwrap();
        ws.focus_row(1);
        ws.insert_window(2, Some(800)).unwrap();
        ws.focus_row(2);
        ws.insert_window(3, Some(800)).unwrap();

        assert_eq!(ws.row_count(), 3);
        assert_eq!(ws.window_count(), 3);

        // Shrink to 1 row: rows 1 and 2 are drained into row 0
        ws.set_row_layout(&[1.0]);
        assert_eq!(ws.row_count(), 1);
        assert_eq!(ws.window_count(), 3);
        assert_eq!(ws.window_count_for_row(0), 3);
        assert!(ws.contains_window(1));
        assert!(ws.contains_window(2));
        assert!(ws.contains_window(3));

        // Expand back to 2 rows: new row is empty
        ws.set_row_layout(&[1.0, 1.0]);
        assert_eq!(ws.row_count(), 2);
        assert_eq!(ws.window_count_for_row(0), 3);
        assert_eq!(ws.window_count_for_row(1), 0);
    }

    #[test]
    fn test_legacy_json_deserialization_single_row() {
        let legacy_json = r#"{
            "columns": [
                {
                    "windows": [101, 102],
                    "width": 800,
                    "height_weights": [0.5, 0.5],
                    "mode": { "type": "vertical" }
                }
            ],
            "focused_column": 0,
            "focused_window_in_column": 1,
            "scroll_offset": 25.0,
            "gap": 12,
            "default_column_width": 800
        }"#;

        let ws: Workspace = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(ws.row_count(), 1);
        assert_eq!(ws.focused_row(), 0);
        assert_eq!(ws.columns().len(), 1);
        assert_eq!(ws.columns()[0].len(), 2);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window_in_column(), 1);
        assert_eq!(ws.scroll_offset(), 25.0);
        assert_eq!(ws.gap(), 12);
    }

    #[test]
    fn test_multi_row_json_roundtrip() {
        let mut ws = Workspace::with_gaps(10, 10);
        ws.set_row_layout(&[0.4, 0.6]);
        ws.set_row_gap(16);

        ws.focus_row(0);
        ws.insert_window(10, Some(750)).unwrap();

        ws.focus_row(1);
        ws.insert_window(20, Some(850)).unwrap();
        ws.insert_window(30, Some(900)).unwrap();

        let json = serde_json::to_string_pretty(&ws).unwrap();
        let restored: Workspace = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.row_count(), 2);
        assert_eq!(restored.row_shares(), &[0.4, 0.6]);
        assert_eq!(restored.row_gap(), 16);
        assert_eq!(restored.focused_row(), 1);
        assert_eq!(restored.window_count_for_row(0), 1);
        assert_eq!(restored.window_count_for_row(1), 2);
        assert_eq!(restored.find_window_location_rc(10), Some((0, 0, 0)));
        assert_eq!(restored.find_window_location_rc(20), Some((1, 0, 0)));
        assert_eq!(restored.find_window_location_rc(30), Some((1, 1, 0)));
    }

    #[test]
    fn test_tabbed_column_no_wrap_multi_row() {
        // Single row: tabbed column wraps
        let mut ws_single = Workspace::new();
        ws_single.insert_window(1, None).unwrap();
        ws_single.insert_window_in_column(2, 0).unwrap();
        ws_single.insert_window_in_column(3, 0).unwrap();
        ws_single.toggle_focused_column_tabbed_mode();
        assert_eq!(ws_single.focused_window(), Some(1));
        ws_single.focus_up();
        assert_eq!(
            ws_single.focused_window(),
            Some(3),
            "Single row tabbed wraps up to last tab"
        );
        ws_single.focus_down();
        assert_eq!(
            ws_single.focused_window(),
            Some(1),
            "Single row tabbed wraps down to first tab"
        );

        // Multi row: tabbed column does NOT wrap, leaves for adjacent row
        let mut ws = Workspace::new();
        ws.set_row_layout(&[0.5, 0.5]);

        // Row 0: tabbed column with windows 1, 2
        ws.focus_row(0);
        ws.insert_window(1, None).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.toggle_focused_column_tabbed_mode();

        // Row 1: tabbed column with windows 3, 4
        ws.focus_row(1);
        ws.insert_window(3, None).unwrap();
        ws.insert_window_in_column(4, 0).unwrap();
        ws.toggle_focused_column_tabbed_mode();

        // In Row 0 at tab 0 (win 1): focus_up must not wrap, stays at tab 0 (top of workspace)
        ws.focus_row(0);
        assert_eq!(ws.focused_window(), Some(1));
        ws.focus_up();
        assert_eq!(
            ws.focused_window(),
            Some(1),
            "Row 0 tab 0 focus_up stays at top, does not wrap"
        );

        // Move to tab 1 (win 2): focus_down leaves Row 0 for Row 1 (lands on Row 1 last focused: win 3)
        ws.focus_down();
        assert_eq!(ws.focused_window(), Some(2));
        ws.focus_down(); // Leaves Row 0 for Row 1
        assert_eq!(ws.focused_row(), 1);
        assert_eq!(ws.focused_window(), Some(3), "Leaves Row 0 for Row 1");

        // In Row 1, focus tab 1 (win 4): focus_down stays at tab 4 (bottom of workspace)
        ws.focus_down();
        assert_eq!(ws.focused_window(), Some(4));
        ws.focus_down();
        assert_eq!(
            ws.focused_window(),
            Some(4),
            "Row 1 last tab focus_down stays at bottom, does not wrap"
        );

        // From tab 0 in Row 1: focus_up leaves Row 1 for Row 0 (lands on win 2)
        ws.focus_up();
        assert_eq!(ws.focused_window(), Some(3));
        ws.focus_up(); // Leaves Row 1 for Row 0
        assert_eq!(ws.focused_row(), 0);
        assert_eq!(
            ws.focused_window(),
            Some(2),
            "Leaves Row 1 for Row 0 last focused"
        );
    }

    #[test]
    fn test_cross_row_focus_lands_on_last_focused() {
        let mut ws = Workspace::new();
        ws.set_row_layout(&[0.5, 0.5]);

        // Row 0: columns with win 1 and win 2
        ws.focus_row(0);
        ws.insert_window(1, None).unwrap();
        ws.insert_window(2, None).unwrap();
        ws.focus_left();
        assert_eq!(ws.focused_window(), Some(1)); // Focus win 1 in row 0

        // Row 1: columns with win 3 and win 4
        ws.focus_row(1);
        ws.insert_window(3, None).unwrap();
        ws.insert_window(4, None).unwrap();
        assert_eq!(ws.focused_window(), Some(4)); // Focus win 4 in row 1

        // Switch to row 0 -> lands on win 1 (last focused in row 0)
        ws.focus_row_up();
        assert_eq!(ws.focused_row(), 0);
        assert_eq!(ws.focused_window(), Some(1));

        // Focus win 2 in row 0
        ws.focus_right();
        assert_eq!(ws.focused_window(), Some(2));

        // focus_down -> leaves row 0, lands on win 4 (last focused in row 1)
        ws.focus_down();
        assert_eq!(ws.focused_row(), 1);
        assert_eq!(ws.focused_window(), Some(4));

        // focus_up -> leaves row 1, lands on win 2 (last focused in row 0)
        ws.focus_up();
        assert_eq!(ws.focused_row(), 0);
        assert_eq!(ws.focused_window(), Some(2));
    }

    #[test]
    fn test_focus_empty_row_and_insert() {
        let mut ws = Workspace::new();
        ws.set_row_layout(&[0.33, 0.33, 0.34]);

        ws.focus_row(0);
        ws.insert_window(10, None).unwrap();

        ws.focus_row(2);
        ws.insert_window(30, None).unwrap();

        // Row 1 is empty
        assert!(ws.is_row_empty(1));

        // Focus row 1 directly
        ws.focus_row(1);
        assert_eq!(ws.focused_row(), 1);
        assert_eq!(ws.focused_window(), None);
        assert_eq!(ws.focused_visible_window(), None);

        // Directional focus_down from row 1 lands on row 2
        ws.focus_down();
        assert_eq!(ws.focused_row(), 2);
        assert_eq!(ws.focused_window(), Some(30));

        // Directional focus_up from row 2 lands on row 1 (empty row)
        ws.focus_up();
        assert_eq!(ws.focused_row(), 1);
        assert_eq!(ws.focused_window(), None);

        // Directional focus_up from row 1 lands on row 0
        ws.focus_up();
        assert_eq!(ws.focused_row(), 0);
        assert_eq!(ws.focused_window(), Some(10));

        // Move to empty row 1 and insert a window
        ws.focus_row_down();
        assert_eq!(ws.focused_row(), 1);
        ws.insert_window(20, None).unwrap();
        assert_eq!(ws.focused_row(), 1);
        assert_eq!(ws.focused_window(), Some(20));
        assert_eq!(ws.find_window_location_rc(20), Some((1, 0, 0)));
    }

    #[test]
    fn test_move_window_edge_across_rows_setting() {
        let mut ws = Workspace::new();
        ws.set_row_layout(&[0.5, 0.5]);

        // Row 0: column 0 with windows 1 and 2 vertically stacked
        ws.focus_row(0);
        ws.insert_window(1, None).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();

        // Row 1: column 0 with window 3
        ws.focus_row(1);
        ws.insert_window(3, None).unwrap();

        ws.focus_row(0);
        ws.focus_up();
        assert_eq!(ws.focused_window(), Some(1)); // win 1 is at top of column

        // At top of column: move_window_up(false) is no-op
        ws.move_window_up(false);
        assert_eq!(ws.focused_window(), Some(1));
        assert_eq!(ws.find_window_location_rc(1), Some((0, 0, 0)));

        // At top of row 0: move_window_up(true) is also no-op (no row above)
        ws.move_window_up(true);
        assert_eq!(ws.find_window_location_rc(1), Some((0, 0, 0)));

        // Move to window 2 (at bottom of column in row 0)
        ws.focus_down();
        assert_eq!(ws.focused_window(), Some(2));

        // move_window_down(false) is a no-op (setting off)
        ws.move_window_down(false);
        assert_eq!(ws.find_window_location_rc(2), Some((0, 0, 1)));

        // move_window_down(true) moves window 2 to row 1 (setting on)
        ws.move_window_down(true);
        assert_eq!(ws.focused_row(), 1);
        assert_eq!(ws.focused_window(), Some(2));
        assert_eq!(ws.find_window_location_rc(2), Some((1, 1, 0)));
        assert_eq!(ws.window_count_for_row(0), 1);
        assert_eq!(ws.window_count_for_row(1), 2);

        // At top of column in row 1: move_window_up(true) moves window 2 back to row 0
        ws.move_window_up(true);
        assert_eq!(ws.focused_row(), 0);
        assert_eq!(ws.focused_window(), Some(2));
        assert_eq!(ws.find_window_location_rc(2), Some((0, 1, 0)));
    }

    #[test]
    fn test_focus_next_prev_row_major_skipping_empty_rows() {
        let mut ws = Workspace::new();
        ws.set_row_layout(&[0.33, 0.33, 0.34]);

        // Row 0: windows 1 and 2 in separate columns
        ws.focus_row(0);
        ws.insert_window(1, None).unwrap();
        ws.insert_window(2, None).unwrap();

        // Row 1: empty!
        // Row 2: column 0 with windows 3 and 4 stacked
        ws.focus_row(2);
        ws.insert_window(3, None).unwrap();
        ws.insert_window_in_column(4, 0).unwrap();

        // Start at window 1 (Row 0, Col 0)
        ws.focus_row(0);
        ws.focus_left();
        assert_eq!(ws.focused_window(), Some(1));

        // Forward cycling via focus_next()
        ws.focus_next();
        assert_eq!(ws.focused_window(), Some(2), "Next from 1 -> 2 (Row 0)");
        ws.focus_next();
        assert_eq!(
            ws.focused_window(),
            Some(3),
            "Next from 2 -> 3 (skips empty Row 1 into Row 2)"
        );
        assert_eq!(ws.focused_row(), 2);
        ws.focus_next();
        assert_eq!(
            ws.focused_window(),
            Some(4),
            "Next from 3 -> 4 (stacked in Row 2)"
        );
        ws.focus_next();
        assert_eq!(
            ws.focused_window(),
            Some(1),
            "Next from 4 wraps to 1 (Row 0)"
        );
        assert_eq!(ws.focused_row(), 0);

        // Reverse cycling via focus_prev()
        ws.focus_prev();
        assert_eq!(
            ws.focused_window(),
            Some(4),
            "Prev from 1 wraps to 4 (Row 2, skipping empty Row 1)"
        );
        assert_eq!(ws.focused_row(), 2);
        ws.focus_prev();
        assert_eq!(ws.focused_window(), Some(3), "Prev from 4 -> 3");
        ws.focus_prev();
        assert_eq!(
            ws.focused_window(),
            Some(2),
            "Prev from 3 -> 2 (Row 0, skipping empty Row 1)"
        );
        assert_eq!(ws.focused_row(), 0);
        ws.focus_prev();
        assert_eq!(ws.focused_window(), Some(1), "Prev from 2 -> 1");
    }

    #[test]
    fn test_multi_row_animation_tick_and_query() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.set_row_layout(&[0.5, 0.5]);

        ws.focus_row(0);
        for id in 1..=5 {
            ws.insert_window(id, Some(500)).unwrap();
        }

        ws.focus_row(1);
        for id in 6..=10 {
            ws.insert_window(id, Some(500)).unwrap();
        }

        assert!(!ws.is_animating());
        assert!(!ws.is_animating_for_row(0));
        assert!(!ws.is_animating_for_row(1));

        // Start animation on row 0
        ws.start_scroll_animation_for_row(0, 300.0, 1000, Some(200), Some(Easing::Linear));
        assert!(ws.is_animating());
        assert!(ws.is_animating_for_row(0));
        assert!(!ws.is_animating_for_row(1));

        // Start animation on row 1
        ws.start_scroll_animation_for_row(1, 400.0, 1000, Some(400), Some(Easing::Linear));
        assert!(ws.is_animating());
        assert!(ws.is_animating_for_row(0));
        assert!(ws.is_animating_for_row(1));

        // Tick 250ms: row 0 finishes (duration 200), row 1 still active (duration 400)
        let still_running = ws.tick_animation(250);
        assert!(still_running);
        assert!(!ws.is_animating_for_row(0));
        assert!(ws.is_animating_for_row(1));
        assert_eq!(ws.rows[0].scroll_offset, 300.0);

        // Tick another 200ms: row 1 finishes
        let still_running = ws.tick_animation(200);
        assert!(!still_running);
        assert!(!ws.is_animating());
        assert_eq!(ws.rows[1].scroll_offset, 400.0);
    }

    #[test]
    fn test_at_workspace_top_and_bottom() {
        let mut ws = Workspace::new();
        ws.set_row_layout(&[0.5, 0.5]);

        ws.focus_row(0);
        ws.insert_window(1, None).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();

        ws.focus_row(1);
        ws.insert_window(3, None).unwrap();
        ws.insert_window_in_column(4, 0).unwrap();

        // Row 0, win 1
        ws.focus_row(0);
        ws.focus_up();
        assert_eq!(ws.focused_window(), Some(1));
        assert!(ws.at_column_top());
        assert!(ws.at_workspace_top());
        assert!(!ws.at_column_bottom());
        assert!(!ws.at_workspace_bottom());

        // Row 0, win 2
        ws.focus_down();
        assert_eq!(ws.focused_window(), Some(2));
        assert!(!ws.at_column_top());
        assert!(!ws.at_workspace_top());
        assert!(ws.at_column_bottom());
        assert!(
            !ws.at_workspace_bottom(),
            "Row 0 column bottom is not workspace bottom"
        );

        // Row 1, win 3
        ws.focus_row(1);
        assert_eq!(ws.focused_window(), Some(3));
        assert!(ws.at_column_top());
        assert!(
            !ws.at_workspace_top(),
            "Row 1 column top is not workspace top"
        );
        assert!(!ws.at_column_bottom());
        assert!(!ws.at_workspace_bottom());

        // Row 1, win 4
        ws.focus_down();
        assert_eq!(ws.focused_window(), Some(4));
        assert!(!ws.at_column_top());
        assert!(!ws.at_workspace_top());
        assert!(ws.at_column_bottom());
        assert!(ws.at_workspace_bottom());
    }

    #[test]
    fn test_set_row_layout_shrink_preserves_or_restores_maximized_column() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.set_row_layout(&[0.5, 0.5]);
        ws.focus_row(0);
        ws.insert_window(1, Some(400)).unwrap();
        ws.focus_row(1);
        ws.insert_window(2, Some(500)).unwrap();
        assert!(ws.toggle_maximize_column(1920));
        assert_eq!(ws.rows()[1].columns()[0].width(), 1920);

        // Shrink to 1 row: row 1's maximized column moves to row 0 and can be un-maximized.
        ws.set_row_layout(&[1.0]);
        assert_eq!(ws.row_count(), 1);
        assert_eq!(ws.rows()[0].columns()[1].width(), 1920);
        assert!(!ws.toggle_maximize_column(1920));
        assert_eq!(ws.rows()[0].columns()[1].width(), 500);

        // If both rows had a maximized column when shrinking, the second one restores its original width immediately.
        ws.set_row_layout(&[0.5, 0.5]);
        ws.move_window_to_row(2, 1).unwrap();
        ws.focus_row(0);
        assert!(ws.toggle_maximize_column(1920));
        ws.focus_row(1);
        assert!(ws.toggle_maximize_column(1920));
        ws.set_row_layout(&[1.0]);
        assert_eq!(ws.rows()[0].columns()[0].width(), 1920);
        assert_eq!(ws.rows()[0].columns()[1].width(), 500);
        assert!(!ws.toggle_maximize_column(1920));
        assert_eq!(ws.rows()[0].columns()[0].width(), 400);
    }

    #[test]
    fn test_move_first_window_of_maximized_column_to_another_row_preserves_restore() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.set_row_layout(&[0.5, 0.5]);
        ws.focus_row(0);
        ws.insert_window(1, Some(450)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        assert!(ws.toggle_maximize_column(1920));
        assert_eq!(ws.rows()[0].columns()[0].width(), 1920);

        // Move the sentinel window (1) to row 1.
        ws.move_window_to_row(1, 1).unwrap();
        // The moved window in row 1 gets the original un-maximized width (450), not 1920.
        assert_eq!(ws.rows()[1].columns()[0].width(), 450);
        // The remaining window (2) in row 0 stays maximized and can still be restored to 450!
        assert_eq!(ws.rows()[0].columns()[0].width(), 1920);
        ws.focus_row(0);
        assert!(!ws.toggle_maximize_column(1920));
        assert_eq!(ws.rows()[0].columns()[0].width(), 450);
    }

    #[test]
    fn test_equalize_column_widths_only_clears_focused_row_min_sizes() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.set_row_layout(&[0.5, 0.5]);
        ws.focus_row(0);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.set_window_min_width(1, 600);

        ws.focus_row(1);
        ws.insert_window(3, Some(400)).unwrap();
        ws.set_window_min_width(3, 700);
        ws.set_window_min_height(3, 350);

        ws.focus_row(0);
        ws.equalize_column_widths(1200);
        assert_eq!(ws.window_min_widths.get(&1).copied(), None);
        assert_eq!(ws.window_min_widths.get(&3).copied(), Some(700));
        assert_eq!(ws.window_min_height(3), Some(350));
    }

    #[test]
    fn test_land_focus_in_row_clamps_window_index_when_remembered_column_minimized() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.set_row_layout(&[0.5, 0.5]);
        ws.focus_row(0);
        // Column A has 1 window; Column B has 3 windows.
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();
        ws.insert_window_in_column(3, 1).unwrap();
        ws.insert_window_in_column(4, 1).unwrap();
        // Focus is on the third window of Column B (index 2).
        ws.set_focus(1, 2).unwrap();
        assert_eq!(ws.focused_window(), Some(4));

        // Focus row 1, minimize all windows in Column B, then focus row 0.
        ws.focus_row(1);
        ws.insert_window(5, Some(400)).unwrap();
        ws.mark_minimized(2);
        ws.mark_minimized(3);
        ws.mark_minimized(4);

        ws.focus_row(0);
        assert_eq!(ws.focused_row(), 0);
        assert_eq!(ws.focused_column_index(), 0);
        assert_eq!(ws.focused_window_in_column(), 0);
        assert_eq!(ws.focused_window(), Some(1));
    }

    #[test]
    fn test_move_window_up_down_on_empty_row_does_not_panic() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.set_row_layout(&[0.5, 0.5]);
        ws.focus_row(0);
        assert!(ws.rows()[0].is_empty());
        ws.move_window_up(false);
        ws.move_window_up(true);
        ws.move_window_down(false);
        ws.move_window_down(true);
        ws.move_window_up_in_column();
        ws.move_window_down_in_column();
    }

    #[test]
    fn test_toggle_maximize_column_exits_fullscreen() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(500)).unwrap();
        ws.set_window_min_width(1, 600);
        ws.set_window_min_height(1, 400);
        assert!(ws.toggle_fullscreen());
        assert_eq!(ws.fullscreen_window_id(), Some(1));

        assert!(ws.toggle_maximize_column(1920));
        assert_eq!(ws.fullscreen_window_id(), None);
        assert_eq!(ws.window_min_widths.get(&1).copied(), None);
        assert_eq!(ws.window_min_height(1), None);
    }

    #[test]
    fn test_resize_snap_preview_and_release_agree_in_multi_row_with_outer_gaps() {
        let mut ws = Workspace::with_directional_gaps(10, 20, 20, 40, 40);
        ws.set_row_gap(20);
        ws.set_row_layout(&[0.5, 0.5]);
        ws.focus_row(1);
        ws.insert_window(10, Some(600)).unwrap();
        ws.insert_window_in_column(11, 0).unwrap();

        let viewport = Rect::new(0, 0, 1920, 1080);
        // Total usable vertical height across rows = 1080 - 40 - 40 - 20 = 980 -> each row is 490px.
        // In row 1, 2 stacked windows have 1 gap of 10px -> available column height = 480px.
        // 2/3 of 480px = 320px.
        let width_presets = [0.333, 0.5, 0.667];
        let height_presets = [0.333, 0.5, 0.667];

        let preview = ws
            .preview_resize_snap(10, 900, 315, &width_presets, &height_presets, viewport)
            .unwrap();

        ws.snap_column_width_to_preset_in_row(1, 0, 900, &width_presets, viewport.width);
        ws.snap_window_height_to_preset_in_row(1, 0, 0, 315, &height_presets, viewport.height);
        let released = ws
            .compute_placements(viewport)
            .into_iter()
            .find(|p| p.window_id == 10)
            .unwrap()
            .rect;

        assert_eq!(preview, released);
        assert!((ws.rows()[1].columns()[0].height_weights()[0] - 0.667).abs() < 1e-6);
    }

    #[test]
    fn test_focus_next_prev_across_rows_lands_on_active_tab() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.set_row_layout(&[0.5, 0.5]);

        ws.focus_row(0);
        ws.insert_window(1, Some(600)).unwrap();
        ws.insert_window(2, Some(600)).unwrap();
        ws.insert_window_in_column(3, 1).unwrap();
        ws.insert_window_in_column(4, 1).unwrap();
        ws.set_focus(1, 1).unwrap();
        ws.toggle_focused_column_tabbed_mode();
        assert_eq!(ws.rows()[0].columns()[1].active_tab_idx(), Some(1));

        ws.focus_row(1);
        ws.insert_window(5, Some(600)).unwrap();
        ws.insert_window_in_column(6, 0).unwrap();
        ws.insert_window_in_column(7, 0).unwrap();
        ws.set_focus(0, 2).unwrap();
        ws.toggle_focused_column_tabbed_mode();
        assert_eq!(ws.rows()[1].columns()[0].active_tab_idx(), Some(2));

        // Start at row 0, col 0 (window 1).
        ws.focus_row(0);
        ws.set_focus(0, 0).unwrap();
        assert_eq!(ws.focused_window(), Some(1));

        // focus_next into same-row tabbed column lands on active tab (window 3, index 1).
        ws.focus_next();
        assert_eq!(ws.focused_row(), 0);
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(3));
        assert_eq!(ws.rows()[0].columns()[1].active_tab_idx(), Some(1));

        // Next advances to window 4 inside that tabbed column, then next wraps to row 1's tabbed column on its active tab (window 7, index 2).
        ws.focus_next();
        assert_eq!(ws.focused_window(), Some(4));
        ws.focus_next();
        assert_eq!(ws.focused_row(), 1);
        assert_eq!(ws.focused_window(), Some(7));
        assert_eq!(ws.rows()[1].columns()[0].active_tab_idx(), Some(2));

        // Reset row 0 col 1 active tab to index 1 (window 3), then focus_prev from row 1 col 0 index 0 (window 5).
        ws.focus_row(0);
        ws.set_active_tab(1, 1).unwrap();
        ws.focus_row(1);
        ws.set_focus(0, 0).unwrap();
        ws.focus_prev();
        assert_eq!(ws.focused_row(), 0);
        assert_eq!(ws.focused_column_index(), 1);
        assert_eq!(ws.focused_window(), Some(3));
        assert_eq!(ws.rows()[0].columns()[1].active_tab_idx(), Some(1));
    }

    #[test]
    fn test_maximized_column_state_transferred_or_cleared_on_intra_row_moves() {
        // 1. expel_to_right from a maximized stacked column transfers sentinel and uses original_width
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(500)).unwrap();
        ws.insert_window_in_column(2, 0).unwrap();
        ws.set_focus(0, 0).unwrap();
        assert!(ws.toggle_maximize_column(1920));
        assert_eq!(ws.columns()[0].width(), 1920);

        ws.expel_to_right();
        assert_eq!(ws.column_count(), 2);
        // Remaining column (window 2) stays maximized with sentinel transferred to window 2.
        assert_eq!(ws.columns()[0].windows(), &[2]);
        assert_eq!(ws.columns()[0].width(), 1920);
        // Expelled column (window 1) gets original_width (500), not 1920!
        assert_eq!(ws.columns()[1].windows(), &[1]);
        assert_eq!(ws.columns()[1].width(), 500);
        // Toggling maximize on column 0 restores it to 500.
        ws.set_focus(0, 0).unwrap();
        assert!(!ws.toggle_maximize_column(1920));
        assert_eq!(ws.columns()[0].width(), 500);

        // 2. move_window_left emptying a maximized column clears maximized_column
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(600)).unwrap();
        ws.set_focus(1, 0).unwrap();
        assert!(ws.toggle_maximize_column(1920));
        ws.move_window_left();
        assert_eq!(ws.column_count(), 1);
        assert!(ws.rows()[0].maximized_column.is_none());

        // 3. consume_from_right pulling the sentinel from a 2-window maximized column transfers sentinel
        let mut ws = Workspace::with_gaps(0, 0);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(600)).unwrap();
        ws.insert_window_in_column(3, 1).unwrap();
        ws.set_focus(1, 0).unwrap();
        assert!(ws.toggle_maximize_column(1920));
        ws.set_focus(0, 0).unwrap();
        ws.consume_from_right();
        assert_eq!(ws.columns()[0].windows(), &[1, 2]);
        assert_eq!(ws.columns()[1].windows(), &[3]);
        assert_eq!(
            ws.rows()[0].maximized_column.as_ref().map(|m| m.sentinel_window),
            Some(3)
        );

        // 4. remove_column_in_row clears maximized_column when removing the maximized column
        let removed = ws.remove_column_in_row(0, 1);
        assert!(removed.is_some());
        assert!(ws.rows()[0].maximized_column.is_none());
    }

    #[test]
    fn test_set_row_layout_shrink_preserves_focused_window_in_drained_row() {
        let mut ws = Workspace::with_gaps(0, 0);
        ws.set_row_layout(&[0.5, 0.5]);
        ws.focus_row(0);
        ws.insert_window(1, Some(400)).unwrap();
        ws.insert_window(2, Some(400)).unwrap();

        ws.focus_row(1);
        ws.insert_window(10, Some(400)).unwrap();
        ws.insert_window(11, Some(400)).unwrap();
        ws.insert_window_in_column(12, 1).unwrap();
        ws.set_focus(1, 1).unwrap();
        assert_eq!(ws.focused_window(), Some(12));

        // Shrink from 2 rows to 1 row while focus is in row 1 on window 12 (col 1, win 1).
        ws.set_row_layout(&[1.0]);
        assert_eq!(ws.row_count(), 1);
        assert_eq!(ws.focused_row(), 0);
        assert_eq!(ws.focused_column_index(), 3);
        assert_eq!(ws.focused_window_index_in_column(), 1);
        assert_eq!(ws.focused_window(), Some(12));
    }

    #[test]
    fn test_workspace_deserialization_caps_rows_at_max_rows_and_normalizes_shares() {
        let json = r#"{
            "rows": [
                {"columns": [], "focused_column": 0, "focused_window_in_column": 0, "scroll_offset": 0.0},
                {"columns": [], "focused_column": 0, "focused_window_in_column": 0, "scroll_offset": 0.0},
                {"columns": [], "focused_column": 0, "focused_window_in_column": 0, "scroll_offset": 0.0},
                {"columns": [], "focused_column": 0, "focused_window_in_column": 0, "scroll_offset": 0.0},
                {"columns": [{"windows": [99], "width": 500}], "focused_column": 0, "focused_window_in_column": 0, "scroll_offset": 0.0}
            ],
            "row_shares": [1.0],
            "focused_row": 4,
            "row_gap": -10,
            "floating": [],
            "gap": -5,
            "outer_gap_left": 0,
            "outer_gap_right": 0,
            "outer_gap_top": 0,
            "outer_gap_bottom": 0,
            "default_column_width": 10
        }"#;
        let ws: Workspace = serde_json::from_str(json).unwrap();
        assert_eq!(ws.row_count(), MAX_ROWS);
        assert_eq!(ws.row_shares().len(), MAX_ROWS);
        assert_eq!(ws.focused_row(), MAX_ROWS - 1);
        assert_eq!(ws.row_gap(), 0);
        assert_eq!(ws.gap(), 0);
        // Window 99 from the 5th row was merged into the 4th row (index 3).
        assert_eq!(ws.rows()[MAX_ROWS - 1].columns()[0].windows(), &[99]);
    }
}
