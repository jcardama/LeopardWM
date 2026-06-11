//! Workspace overview: a fullscreen map of the focused monitor's
//! non-empty workspaces, one miniaturized row per workspace.
//!
//! `build_overview_model` is pure with respect to the platform — it reads
//! workspace state and computes geometry only — so the model is unit
//! tested without ever constructing the overlay window. `show_overview`
//! lazily creates the [`OverviewOverlay`] on first use (never in
//! `AppState::new`, never under `cfg(test)`).

use crate::state::AppState;
use leopardwm_core_layout::{Rect, Visibility, Workspace};
use leopardwm_platform_win32::overview::{
    OverviewCard, OverviewModel, OverviewOverlay, OverviewRow, DEFAULT_ACCENT_BGR,
    PANEL_INNER_PAD, SELECT_PAD, VIEWPORT_RING_PAD,
};
use tracing::{info, warn};

/// Height of the label row across the top of each workspace panel.
const LABEL_STRIP_H: i32 = 24;
/// Outer margin as a fraction of the work-area dimensions.
const OUTER_MARGIN_FRAC: f64 = 0.04;
/// Vertical gap between rows as a fraction of work-area height.
const ROW_GAP_FRAC: f64 = 0.02;
/// Padding between a row panel's edge and the viewport ring (the
/// renderer folds the same value into the panel's corner radius).
const ROW_INNER_PAD: i32 = PANEL_INNER_PAD;
/// Body inset from the panel edge (and label strip) to the miniaturized
/// strip content: panel padding plus the viewport-ring breathing room, so
/// the ring (drawn VIEWPORT_RING_PAD outside the viewport region) never
/// clips the panel or overlaps the label strip.
const BODY_INSET: i32 = ROW_INNER_PAD + VIEWPORT_RING_PAD;

/// MAXIMUM geometry for one overview row, all in overlay client
/// coordinates. The final panel/label-strip rects wrap the actual scaled
/// strip content (left-aligned), so these are upper bounds on width.
struct OverviewRowGeometry {
    panel: Rect,
    label_strip: Rect,
    strip: Rect,
}

/// Lay out `n` row slots top-to-bottom inside a `w` x `h` client area:
/// 4% outer margin, 2% row gap, equal-height rows capped at `h / 3` and
/// centered vertically when fewer rows than would fill the area.
fn layout_overview_rows(w: i32, h: i32, n: usize) -> Vec<OverviewRowGeometry> {
    let n_i = n as i32;
    if n_i <= 0 || w <= 0 || h <= 0 {
        return Vec::new();
    }
    // The active-panel accent ring is drawn SELECT_PAD px outside its
    // panel: keep margins and the row gap large enough that the ring
    // never clips the overlay bounds or a neighboring row.
    let margin_x = ((f64::from(w) * OUTER_MARGIN_FRAC).round() as i32).max(SELECT_PAD + 2);
    let margin_y = ((f64::from(h) * OUTER_MARGIN_FRAC).round() as i32).max(SELECT_PAD + 2);
    let gap = ((f64::from(h) * ROW_GAP_FRAC).round() as i32).max(2 * SELECT_PAD + 2);
    let avail_h = h - 2 * margin_y - gap * (n_i - 1);
    let row_h = (avail_h / n_i).min(h / 3).max(1);
    let total_h = row_h * n_i + gap * (n_i - 1);
    let top = margin_y + ((h - 2 * margin_y) - total_h).max(0) / 2;
    let panel_w = (w - 2 * margin_x).max(1);
    let label_h = LABEL_STRIP_H.min(row_h);
    (0..n_i)
        .map(|i| {
            let y = top + i * (row_h + gap);
            OverviewRowGeometry {
                panel: Rect::new(margin_x, y, panel_w, row_h),
                label_strip: Rect::new(margin_x, y, panel_w, label_h),
                strip: Rect::new(
                    margin_x + BODY_INSET,
                    y + label_h + BODY_INSET,
                    (panel_w - 2 * BODY_INSET).max(1),
                    (row_h - label_h - 2 * BODY_INSET).max(1),
                ),
            }
        })
        .collect()
}

/// Gap carved symmetrically off each scaled card so neighbors don't touch.
const CARD_INSET: i32 = 2;

/// Uniform full-strip → row-strip transform: one scale factor for both
/// axes and one offset for every rect, so relative window geometry is
/// preserved exactly (no per-card clamping or fitting).
struct StripTransform {
    scale: f64,
    min_x: i32,
    min_y: i32,
    offset_x: i32,
    offset_y: i32,
}

impl StripTransform {
    /// Fit `content` (full-strip virtual coordinates) into `strip`,
    /// preserving aspect ratio: left-aligned (the row panel wraps the
    /// scaled content), centered vertically.
    fn fit(content: &Rect, strip: &Rect) -> Self {
        let content_w = f64::from(content.width.max(1));
        let content_h = f64::from(content.height.max(1));
        let scale =
            (f64::from(strip.height) / content_h).min(f64::from(strip.width) / content_w);
        StripTransform {
            scale,
            min_x: content.x,
            min_y: content.y,
            offset_x: strip.x,
            offset_y: strip.y
                + ((f64::from(strip.height) - content_h * scale) / 2.0).round() as i32,
        }
    }

    /// Width of the scaled content, i.e. how much of the strip's width
    /// the content actually occupies after fitting.
    fn scaled_width(&self, content: &Rect) -> i32 {
        (f64::from(content.width.max(1)) * self.scale).round().max(1.0) as i32
    }

    fn apply(&self, r: &Rect) -> Rect {
        Rect::new(
            self.offset_x + (f64::from(r.x - self.min_x) * self.scale).round() as i32,
            self.offset_y + (f64::from(r.y - self.min_y) * self.scale).round() as i32,
            (f64::from(r.width) * self.scale).round().max(1.0) as i32,
            (f64::from(r.height) * self.scale).round().max(1.0) as i32,
        )
    }
}

/// Bounding box of `rects` (assumed non-empty checked by the caller).
fn bounding_box(rects: impl Iterator<Item = Rect>) -> Rect {
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for r in rects {
        min_x = min_x.min(r.x);
        min_y = min_y.min(r.y);
        max_x = max_x.max(r.x + r.width);
        max_y = max_y.max(r.y + r.height);
    }
    Rect::new(min_x, min_y, (max_x - min_x).max(1), (max_y - min_y).max(1))
}

/// Shrink `r` symmetrically by up to `CARD_INSET` per side, never below 1px.
fn inset_card(r: &Rect) -> Rect {
    let ix = CARD_INSET.min((r.width - 1) / 2).max(0);
    let iy = CARD_INSET.min((r.height - 1) / 2).max(0);
    Rect::new(r.x + ix, r.y + iy, r.width - 2 * ix, r.height - 2 * iy)
}

impl AppState {
    /// Build the overview display model for the focused monitor.
    ///
    /// Returns the overlay window rect (the monitor's work area) plus the
    /// model, or `None` when every workspace on the monitor is empty.
    pub(crate) fn build_overview_model(&self) -> Option<(Rect, OverviewModel)> {
        let monitor = self.focused_monitor;
        let work_area = self.monitors.get(&monitor)?.work_area;
        let ws_vec = self.workspaces.get(&monitor)?;
        let active_idx = self.active_workspace_idx(monitor);

        let non_empty: Vec<usize> = ws_vec
            .iter()
            .enumerate()
            .filter(|(_, ws)| ws.window_count() + ws.floating_count() > 0)
            .map(|(i, _)| i)
            .collect();
        if non_empty.is_empty() {
            return None;
        }

        let geoms = layout_overview_rows(work_area.width, work_area.height, non_empty.len());
        let focused_wid = ws_vec.get(active_idx).and_then(|ws| ws.focused_window());
        let accent_width = self.config.appearance.active_border_width.max(1);

        let mut rows = Vec::with_capacity(non_empty.len());
        for (slot, &ws_idx) in non_empty.iter().enumerate() {
            let ws = &ws_vec[ws_idx];
            let geom = &geoms[slot];
            let is_active = ws_idx == active_idx;
            let selected_wid = if is_active { focused_wid } else { None };
            // Vertical symmetry: the 1px panel frame is stroked INSIDE
            // the panel's bottom edge (the active accent ring sits
            // OUTSIDE) while the top boundary is the label band's
            // unstroked bottom edge — trim the strip by the frame width
            // so the ring clears both by the same ROW_INNER_PAD.
            let strip = Rect::new(
                geom.strip.x,
                geom.strip.y,
                geom.strip.width,
                (geom.strip.height - 1).max(1),
            );
            let (mut cards, viewport, content_w) =
                self.overview_cards_for(ws, work_area, strip, selected_wid, is_active);
            // The active row always carries a selection: fall back to its
            // first card when the focused window isn't represented.
            if is_active && !cards.iter().any(|c| c.selected) {
                if let Some(first) = cards.first_mut() {
                    first.selected = true;
                }
            }
            let label = match self.config.workspaces.name_for(ws_idx) {
                Some(name) => format!("{} {}", ws_idx + 1, name),
                None => format!("{}", ws_idx + 1),
            };
            // The panel wraps its content: scaled strip width + inner
            // padding, left-aligned at the outer margin; the label strip
            // shares the panel width. Height per row is unchanged.
            let panel_w = match content_w {
                Some(cw) => (cw + 2 * BODY_INSET).min(geom.panel.width),
                None => geom.panel.width, // no visible content: keep full width
            };
            let panel = Rect::new(geom.panel.x, geom.panel.y, panel_w, geom.panel.height);
            let label_strip =
                Rect::new(panel.x, panel.y, panel_w, geom.label_strip.height);
            rows.push(OverviewRow {
                workspace_index: ws_idx,
                label,
                is_active,
                panel,
                label_strip,
                viewport,
                cards,
            });
        }

        let model = OverviewModel {
            backdrop: Rect::new(0, 0, work_area.width, work_area.height),
            accent_bgr: self.border_color_bgr().unwrap_or(DEFAULT_ACCENT_BGR),
            // Raw config width (not DPI-scaled): the overlay is already
            // sized in the monitor's pixel space.
            accent_width,
            rows,
        };
        Some((work_area, model))
    }

    /// Miniaturize one workspace's ENTIRE strip into `strip` (scrolled-out
    /// columns included), plus a marker rect for the region currently in
    /// the real viewport (zero-sized when every window already fits the
    /// viewport) and the scaled content width (None when the workspace
    /// has no visible windows) so the caller can wrap the panel.
    ///
    /// Tiled placements come from `placements_for_full_strip` (pure — no
    /// state mutation) against a virtual viewport at origin, wide enough
    /// for every column. Floating windows are viewport-anchored, so they
    /// are translated by `scroll_offset` to sit over the viewport region.
    /// Every rect goes through ONE uniform [`StripTransform`] — same
    /// scale, same offset — so relative geometry is exact.
    ///
    /// A fullscreen workspace collapses to ONE viewport-sized card for
    /// the fullscreen window instead of the underlying strip layout.
    fn overview_cards_for(
        &self,
        ws: &Workspace,
        work_area: Rect,
        strip: Rect,
        selected_wid: Option<u64>,
        is_active: bool,
    ) -> (Vec<OverviewCard>, Rect, Option<i32>) {
        let full_w = ws.total_width().saturating_add(work_area.width).max(1);
        let virtual_viewport = Rect::new(0, 0, full_w, work_area.height);
        let scroll = ws.scroll_offset().round() as i32;
        // The strip region the real viewport currently shows.
        let viewport_region = Rect::new(scroll, 0, work_area.width, work_area.height);

        let fullscreen = ws
            .fullscreen_window_id()
            .filter(|&wid| ws.contains_window(wid) && !ws.is_minimized(wid));
        let sources: Vec<(u64, Rect, Option<usize>)> = if let Some(fs_wid) = fullscreen {
            // The fullscreen window covers the whole viewport.
            vec![(fs_wid, viewport_region, None)]
        } else {
            let mut sources: Vec<(u64, Rect, Option<usize>)> = ws
                .placements_for_full_strip(virtual_viewport)
                .into_iter()
                .filter(|p| {
                    p.visibility == Visibility::Visible
                        && p.window_id != crate::state::DRAG_PLACEHOLDER_HWND
                        && p.column_index != usize::MAX // floats handled below
                })
                .map(|p| {
                    let tab_count = ws
                        .column(p.column_index)
                        .filter(|c| c.is_tabbed() && c.len() > 1)
                        .map(leopardwm_core_layout::Column::len);
                    (p.window_id, p.rect, tab_count)
                })
                .collect();
            for f in ws.floating_windows() {
                sources.push((
                    f.id,
                    Rect::new(
                        f.rect.x - work_area.x + scroll,
                        f.rect.y - work_area.y,
                        f.rect.width,
                        f.rect.height,
                    ),
                    None,
                ));
            }
            sources
        };
        if sources.is_empty() {
            return (Vec::new(), Rect::new(strip.x, strip.y, 0, 0), None);
        }

        // Cards entirely outside the viewport must not sit flush against
        // (and cover) the viewport ring: shift them away by a boundary
        // gap — ring pad + ring stroke + 4px, in scaled pixels, applied
        // post-transform. The fit reserves that width up front (and the
        // returned content width includes it) so nothing clips and the
        // panel wrap grows to match.
        // The viewport ring (and its boundary gap) only belongs to the
        // ACTIVE workspace's row; inactive rows show the plain strip.
        let boundary_gap = if is_active {
            VIEWPORT_RING_PAD + self.config.appearance.active_border_width.max(1) as i32 + 4
        } else {
            0
        };
        let vp_right = viewport_region.x + viewport_region.width;
        // In/out classification in RAW strip space, against the viewport
        // region's CONTENT edges: the outer gaps are dead zones (the
        // engine cloaks a column whose strip-x reaches the padded edge),
        // so a card whose left edge sits at/after `vp_right - outer_right`
        // is out of view even though it nominally overlaps the viewport
        // rect. EDGE_EPSILON absorbs the scroll-offset rounding.
        const EDGE_EPSILON: i32 = 1;
        let (outer_left, outer_right, _, _) = ws.outer_gaps();
        let vis_left = viewport_region.x + outer_left.max(0);
        let vis_right = vp_right - outer_right.max(0);
        let out_dir = |r: &Rect| -> i32 {
            if r.x + r.width <= vis_left + EDGE_EPSILON {
                -1
            } else if r.x >= vis_right - EDGE_EPSILON {
                1
            } else {
                0
            }
        };
        let has_left = is_active && sources.iter().any(|(_, r, _)| out_dir(r) < 0);
        let has_right = is_active && sources.iter().any(|(_, r, _)| out_dir(r) > 0);
        // When everything fits the viewport the ring conveys nothing:
        // omit it (and its reserve) so the panel hugs the cards instead
        // of carrying phantom viewport width at the workspace edge.
        let any_out = has_left || has_right;
        let reserve = boundary_gap * (i32::from(has_left) + i32::from(has_right));

        // With the ring shown, content bounds include the viewport region
        // so the ring never escapes the row even when the strip is
        // narrower than the screen.
        let content = if any_out {
            bounding_box(
                sources
                    .iter()
                    .map(|(_, r, _)| *r)
                    .chain(std::iter::once(viewport_region)),
            )
        } else {
            bounding_box(sources.iter().map(|(_, r, _)| *r))
        };
        let fit_strip = Rect::new(
            strip.x,
            strip.y,
            (strip.width - reserve).max(1),
            strip.height,
        );
        let mut xform = StripTransform::fit(&content, &fit_strip);
        if has_left {
            xform.offset_x += boundary_gap;
        }

        let cards = sources
            .into_iter()
            .map(|(wid, r, tab_count)| {
                let mut rect = xform.apply(&r);
                rect.x += out_dir(&r) * boundary_gap;
                OverviewCard {
                    window_id: wid,
                    title: self
                        .lookup_window_info(wid)
                        .map(|i| i.title)
                        .unwrap_or_default(),
                    icon: leopardwm_platform_win32::get_window_icon(wid),
                    rect: inset_card(&rect),
                    tab_count,
                    selected: Some(wid) == selected_wid,
                }
            })
            .collect();
        // The marker is hidden for fullscreen rows (it would coincide
        // with the single card) and when nothing is scrolled out of view
        // (everything visible: "you are here" conveys nothing). Otherwise
        // it is the ring rect: the scaled viewport region inflated by the
        // ring pad so the stroke sits with breathing room around the
        // in-viewport cards.
        let marker = if fullscreen.is_some() || !any_out {
            Rect::new(strip.x, strip.y, 0, 0)
        } else {
            let v = xform.apply(&viewport_region);
            Rect::new(
                v.x - VIEWPORT_RING_PAD,
                v.y - VIEWPORT_RING_PAD,
                v.width + 2 * VIEWPORT_RING_PAD,
                v.height + 2 * VIEWPORT_RING_PAD,
            )
        };
        (cards, marker, Some(xform.scaled_width(&content) + reserve))
    }

    /// Toggle the overview overlay on the focused monitor.
    pub(crate) fn toggle_overview(&mut self) {
        if self.overview_open {
            self.hide_overview();
        } else {
            self.show_overview();
        }
    }

    /// Show the overview. No-op when every workspace is empty. Creates
    /// the overlay lazily on first use (skipped when no event sender is
    /// installed — tests and headless runs).
    pub(crate) fn show_overview(&mut self) {
        let Some((overlay_rect, model)) = self.build_overview_model() else {
            info!("Overview not shown: no non-empty workspaces on the focused monitor");
            return;
        };
        // No ghost animation should keep playing beneath the overlay.
        self.abort_active_ghost_transition();
        if self.overview_overlay.is_none() {
            if let Some(tx) = self.overview_event_tx.clone() {
                match OverviewOverlay::new(tx) {
                    Ok(overlay) => self.overview_overlay = Some(overlay),
                    Err(e) => warn!("Failed to create overview overlay: {}", e),
                }
            }
        }
        if let Some(overlay) = &self.overview_overlay {
            overlay.show(overlay_rect, model);
        }
        self.overview_open = true;
    }

    /// Hide the overview overlay.
    pub(crate) fn hide_overview(&mut self) {
        if let Some(overlay) = &self.overview_overlay {
            overlay.hide();
        }
        self.overview_open = false;
    }

    /// Rebuild and push a fresh model while the overview is open (a
    /// window closed or appeared underneath it). Hides the overview when
    /// the last window on the monitor is gone.
    pub(crate) fn refresh_overview_model(&mut self) {
        if !self.overview_open {
            return;
        }
        match self.build_overview_model() {
            Some((_, model)) => {
                if let Some(overlay) = &self.overview_overlay {
                    overlay.update_model(model);
                }
            }
            None => self.hide_overview(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use leopardwm_platform_win32::MonitorInfo;

    fn test_state() -> AppState {
        AppState::new_with_config(
            Config::default(),
            vec![MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, 1920, 1080),
                work_area: Rect::new(0, 0, 1920, 1040),
                is_primary: true,
                device_name: "DISPLAY1".to_string(),
                scale_factor: 1.0,
            }],
        )
    }

    /// Insert tiled windows into workspace `ws_idx` on monitor 1.
    fn add_windows(state: &mut AppState, ws_idx: usize, wids: &[u64]) {
        let ws = state.ensure_workspace_exists(1, ws_idx).unwrap();
        for &wid in wids {
            ws.insert_window(wid, None).unwrap();
        }
    }

    #[test]
    fn test_build_overview_model_filters_empty_workspaces() {
        let mut state = test_state();
        add_windows(&mut state, 0, &[101]);
        add_windows(&mut state, 2, &[201, 202]);
        // Workspace 1 stays empty and must not produce a row.
        state.ensure_workspace_exists(1, 1);

        let (overlay_rect, model) = state.build_overview_model().expect("model");
        assert_eq!(overlay_rect, Rect::new(0, 0, 1920, 1040));
        let indices: Vec<usize> = model.rows.iter().map(|r| r.workspace_index).collect();
        assert_eq!(indices, vec![0, 2], "non-empty rows in workspace order");
        assert_eq!(model.rows[0].label, "1");
        assert_eq!(model.rows[1].label, "3");
        assert_eq!(model.rows[0].cards.len(), 1);
        assert_eq!(model.rows[1].cards.len(), 2);
    }

    #[test]
    fn test_build_overview_model_returns_none_when_all_empty() {
        let state = test_state();
        assert!(state.build_overview_model().is_none());
    }

    #[test]
    fn test_build_overview_model_marks_active_row_and_selection() {
        let mut state = test_state();
        add_windows(&mut state, 0, &[101, 102]);
        add_windows(&mut state, 2, &[201]);
        state.active_workspace.insert(1, 2);

        let (_, model) = state.build_overview_model().expect("model");
        let active: Vec<bool> = model.rows.iter().map(|r| r.is_active).collect();
        assert_eq!(active, vec![false, true]);
        // Selection lives on the active row only (its focused window).
        assert!(model.rows[1].cards.iter().any(|c| c.selected));
        assert!(model.rows[0].cards.iter().all(|c| !c.selected));
    }

    #[test]
    fn test_overview_cards_stay_within_their_row_panel() {
        let mut state = test_state();
        add_windows(&mut state, 0, &[101, 102, 103]);
        add_windows(&mut state, 1, &[201]);
        // A floating window partially outside the work area must clamp.
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .get_mut(1)
            .unwrap()
            .add_floating(301, Rect::new(1800, 900, 400, 300))
            .unwrap();

        let (_, model) = state.build_overview_model().expect("model");
        for row in &model.rows {
            for card in &row.cards {
                assert!(
                    card.rect.x >= row.panel.x
                        && card.rect.y >= row.panel.y
                        && card.rect.x + card.rect.width <= row.panel.x + row.panel.width
                        && card.rect.y + card.rect.height <= row.panel.y + row.panel.height,
                    "card {:?} escapes panel {:?}",
                    card.rect,
                    row.panel
                );
                assert!(
                    card.rect.y >= row.label_strip.y + row.label_strip.height,
                    "card {:?} overlaps label strip {:?}",
                    card.rect,
                    row.label_strip
                );
            }
        }
    }

    #[test]
    fn test_label_strip_spans_panel_top_and_accent_is_filled() {
        let mut state = test_state();
        add_windows(&mut state, 0, &[101]);
        add_windows(&mut state, 1, &[201]);

        let (_, model) = state.build_overview_model().expect("model");
        for row in &model.rows {
            assert_eq!(row.label_strip.x, row.panel.x, "strip starts at panel left");
            assert_eq!(row.label_strip.y, row.panel.y, "strip starts at panel top");
            assert_eq!(
                row.label_strip.width, row.panel.width,
                "strip spans the panel width"
            );
            assert_eq!(row.label_strip.height, LABEL_STRIP_H);
        }
        // The accent comes from the configured focus-border color (or the
        // platform default when unparseable / high contrast handled there).
        let expected = state.border_color_bgr().unwrap_or(DEFAULT_ACCENT_BGR);
        assert_eq!(model.accent_bgr, expected);
        assert_ne!(model.accent_bgr, 0, "accent must be a real color");
    }

    #[test]
    fn test_overview_maps_whole_strip_including_scrolled_out_columns() {
        let mut state = test_state();
        add_windows(&mut state, 0, &[101, 102, 103, 104]);
        {
            let ws = state.workspaces.get_mut(&1).unwrap().get_mut(0).unwrap();
            ws.set_all_column_widths(1000); // strip ~4000px > 1920 viewport
            ws.set_scroll_offset(2000.0); // first columns scrolled out left
        }

        let (_, model) = state.build_overview_model().expect("model");
        let wids: Vec<u64> = model.rows[0].cards.iter().map(|c| c.window_id).collect();
        assert_eq!(
            wids.len(),
            4,
            "scrolled-out columns must still produce cards, got {:?}",
            wids
        );
        for wid in [101, 102, 103, 104] {
            assert!(wids.contains(&wid), "missing card for window {}", wid);
        }
    }

    #[test]
    fn test_equal_placement_heights_produce_equal_card_heights() {
        let mut state = test_state();
        // Three single-window columns: identical placement heights.
        add_windows(&mut state, 0, &[101, 102, 103]);

        let (_, model) = state.build_overview_model().expect("model");
        let heights: Vec<i32> = model.rows[0].cards.iter().map(|c| c.rect.height).collect();
        assert_eq!(heights.len(), 3);
        assert!(
            heights.windows(2).all(|p| p[0] == p[1]),
            "uniform scale: equal source heights must yield equal card heights, got {:?}",
            heights
        );
        let widths: Vec<i32> = model.rows[0].cards.iter().map(|c| c.rect.width).collect();
        assert!(
            widths.windows(2).all(|p| p[0] == p[1]),
            "uniform scale: equal source widths must yield equal card widths, got {:?}",
            widths
        );
    }

    #[test]
    fn test_tabbed_column_yields_single_card_with_tab_count() {
        let mut state = test_state();
        add_windows(&mut state, 0, &[101]);
        {
            let ws = state.workspaces.get_mut(&1).unwrap().get_mut(0).unwrap();
            ws.insert_window_in_column(102, 0).unwrap();
            ws.insert_window_in_column(103, 0).unwrap();
            ws.toggle_focused_column_tabbed_mode();
            // A second, vertical column for the None contrast case.
            ws.insert_window(104, None).unwrap();
        }

        let (_, model) = state.build_overview_model().expect("model");
        let cards = &model.rows[0].cards;
        assert_eq!(cards.len(), 2, "tabbed column collapses to its active tab");
        let tabbed = cards.iter().find(|c| c.window_id == 101).expect("active tab card");
        assert_eq!(tabbed.tab_count, Some(3));
        let vertical = cards.iter().find(|c| c.window_id == 104).expect("vertical card");
        assert_eq!(vertical.tab_count, None);
    }

    #[test]
    fn test_viewport_marker_tracks_scroll_and_stays_in_panel() {
        let mut state = test_state();
        add_windows(&mut state, 0, &[101, 102, 103, 104]);
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .get_mut(0)
            .unwrap()
            .set_all_column_widths(1000);

        let (_, before) = state.build_overview_model().expect("model");
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .get_mut(0)
            .unwrap()
            .set_scroll_offset(1500.0);
        let (_, after) = state.build_overview_model().expect("model");

        let b = before.rows[0].viewport;
        let a = after.rows[0].viewport;
        assert!(a.width > 0 && a.height > 0, "marker must be non-empty");
        assert!(a.x > b.x, "marker must move right with scroll: {:?} -> {:?}", b, a);
        let panel = after.rows[0].panel;
        assert!(
            a.x >= panel.x
                && a.y >= panel.y
                && a.x + a.width <= panel.x + panel.width
                && a.y + a.height <= panel.y + panel.height,
            "ring {:?} escapes panel {:?}",
            a,
            panel
        );
        let strip_bottom = after.rows[0].label_strip.y + after.rows[0].label_strip.height;
        assert!(
            a.y >= strip_bottom,
            "ring {:?} overlaps label strip ending at y={}",
            a,
            strip_bottom
        );
    }

    #[test]
    fn test_viewport_ring_inflated_with_breathing_room() {
        let mut state = test_state();
        // Strip wider than the viewport so the ring is shown at all.
        add_windows(&mut state, 0, &[101, 102, 103, 104]);
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .get_mut(0)
            .unwrap()
            .set_all_column_widths(1000);

        let (_, model) = state.build_overview_model().expect("model");
        let row = &model.rows[0];
        let ring = row.viewport;
        assert!(ring.width > 0 && ring.height > 0, "ring must be present");
        // Cards fully inside the ring sit with at least the ring pad of
        // breathing room on every side (straddling/out cards excluded).
        let inside: Vec<&OverviewCard> = row
            .cards
            .iter()
            .filter(|c| c.rect.x >= ring.x && c.rect.x + c.rect.width <= ring.x + ring.width)
            .collect();
        assert!(!inside.is_empty(), "expected at least one card inside the ring");
        for card in inside {
            assert!(
                card.rect.x >= ring.x + VIEWPORT_RING_PAD
                    && card.rect.y >= ring.y + VIEWPORT_RING_PAD
                    && card.rect.x + card.rect.width <= ring.x + ring.width - VIEWPORT_RING_PAD
                    && card.rect.y + card.rect.height <= ring.y + ring.height - VIEWPORT_RING_PAD,
                "card {:?} too close to ring {:?}",
                card.rect,
                ring
            );
        }
        // The body insets reserve room: the ring clears the panel edges,
        // the label strip, and the 1px panel frame (the active accent
        // ring sits OUTSIDE the panel now) by the panel padding.
        assert!(ring.x >= row.panel.x + ROW_INNER_PAD);
        assert!(ring.x + ring.width <= row.panel.x + row.panel.width - ROW_INNER_PAD + 2);
        assert!(ring.y >= row.label_strip.y + row.label_strip.height + ROW_INNER_PAD);
        assert!(
            ring.y + ring.height <= row.panel.y + row.panel.height - ROW_INNER_PAD - 1 + 2
        );
    }

    #[test]
    fn test_no_ring_and_no_reserve_when_all_windows_visible() {
        let mut state = test_state();
        // Two default-width columns fit the 1920px viewport entirely.
        add_windows(&mut state, 0, &[101, 102]);

        let (_, model) = state.build_overview_model().expect("model");
        let row = &model.rows[0];
        assert_eq!(row.cards.len(), 2);
        assert_eq!(
            (row.viewport.width, row.viewport.height),
            (0, 0),
            "ring conveys nothing when everything is visible"
        );
        // No boundary reserve and no phantom viewport width: the panel
        // hugs the cards (widest card + inset + body inset).
        let content_right = row
            .cards
            .iter()
            .map(|c| c.rect.x + c.rect.width + CARD_INSET)
            .max()
            .unwrap();
        let panel_right = row.panel.x + row.panel.width;
        assert!(
            (panel_right - content_right - BODY_INSET).abs() <= 2,
            "panel right {} must hug content right {} + body inset (no ring, no reserve)",
            panel_right,
            content_right
        );
    }

    #[test]
    fn test_card_in_outer_gap_dead_zone_classified_out_of_viewport() {
        let mut state = test_state();
        // Default gaps are 10/10: col 1 lands at strip x 1905 + outer
        // left 10 = 1915 — nominally 5px inside the 1920px viewport rect,
        // but past the padded content edge (1920 - outer_right = 1910),
        // so the engine cloaks it. It must classify as OUT and take the
        // boundary gap; the gap must not slide to a later card.
        add_windows(&mut state, 0, &[101, 102]);
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .get_mut(0)
            .unwrap()
            .set_all_column_widths(1895);

        let (_, model) = state.build_overview_model().expect("model");
        let row = &model.rows[0];
        let ring = row.viewport;
        assert!(ring.width > 0, "out-of-view content must show the ring");
        let edge_gap = model.accent_width as i32 + 4;
        let beyond_right: Vec<&OverviewCard> = row
            .cards
            .iter()
            .filter(|c| c.rect.x >= ring.x + ring.width)
            .collect();
        assert_eq!(
            beyond_right.len(),
            1,
            "the dead-zone card must sit beyond the ring, got {:?}",
            row.cards.iter().map(|c| c.rect).collect::<Vec<_>>()
        );
        let out = beyond_right[0];
        assert_eq!(out.window_id, 102);
        // A dead-zone card starts up to outer_gap raw px (~3 scaled px
        // here) inside the nominal viewport edge, so it gives up that
        // much of the boundary gap — it must still clear the ring.
        assert!(
            out.rect.x - CARD_INSET >= ring.x + ring.width + edge_gap - 4,
            "card {:?} too close to ring {:?} (need ~{}px)",
            out.rect,
            ring,
            edge_gap
        );
    }

    #[test]
    fn test_row_layout_reserves_clearance_for_active_panel_ring() {
        // Small client area exercises the SELECT_PAD clamps on the
        // margins and row gap.
        let geoms = layout_overview_rows(400, 120, 3);
        assert_eq!(geoms.len(), 3);
        for g in &geoms {
            assert!(g.panel.x - SELECT_PAD >= 0, "ring clips left edge");
            assert!(g.panel.y - SELECT_PAD >= 0, "ring clips top edge");
        }
        let last = geoms.last().unwrap();
        assert!(
            last.panel.y + last.panel.height + SELECT_PAD <= 120,
            "ring clips bottom edge"
        );
        for pair in geoms.windows(2) {
            let gap = pair[1].panel.y - (pair[0].panel.y + pair[0].panel.height);
            assert!(
                gap >= 2 * SELECT_PAD + 2,
                "row gap {} too small for the outer accent ring",
                gap
            );
        }
    }

    #[test]
    fn test_ring_centers_vertically_in_panel_body() {
        let mut state = test_state();
        state.config.appearance.active_border_width = 4;
        // Strips wider than the viewport so both rows show their ring.
        add_windows(&mut state, 0, &[101, 102, 103, 104]);
        add_windows(&mut state, 1, &[201, 202, 203, 204]);
        for ws_idx in [0, 1] {
            state
                .workspaces
                .get_mut(&1)
                .unwrap()
                .get_mut(ws_idx)
                .unwrap()
                .set_all_column_widths(1000);
        }

        let (_, model) = state.build_overview_model().expect("model");
        for row in &model.rows {
            let ring = row.viewport;
            if !row.is_active {
                // The viewport ring belongs to the active workspace only.
                assert_eq!(ring.height, 0, "inactive rows must not show a ring");
                continue;
            }
            assert!(ring.height > 0, "ring must be present");
            // The panel frame is always 1px inside the bottom edge (the
            // active accent ring sits outside the panel); the top
            // boundary is the label band's unstroked bottom edge.
            let frame = 1;
            let body_top = row.label_strip.y + row.label_strip.height;
            let body_bottom = row.panel.y + row.panel.height - frame;
            let above = ring.y - body_top;
            let below = body_bottom - (ring.y + ring.height);
            assert!(
                (above - below).abs() <= 1,
                "ring not centered in body: {} above vs {} below (active={})",
                above,
                below,
                row.is_active
            );
            assert!(
                (above - ROW_INNER_PAD).abs() <= 1,
                "ring clearance {} must be the panel padding {}",
                above,
                ROW_INNER_PAD
            );
        }
    }

    #[test]
    fn test_cards_outside_viewport_gap_from_ring() {
        let mut state = test_state();
        add_windows(&mut state, 0, &[101, 102, 103, 104]);
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .get_mut(0)
            .unwrap()
            .set_all_column_widths(1000);

        // scroll = 0: trailing columns sit right of the viewport.
        let (_, model) = state.build_overview_model().expect("model");
        let row = &model.rows[0];
        let ring = row.viewport;
        // Boundary gap measured from the ring edge (the daemon shifts
        // out-of-viewport cards by ring pad + accent width + 4 from the
        // viewport edge; the ring edge already sits the pad outside).
        let edge_gap = model.accent_width as i32 + 4;
        let beyond_right: Vec<Rect> = row
            .cards
            .iter()
            .filter(|c| c.rect.x >= ring.x + ring.width)
            .map(|c| c.rect)
            .collect();
        assert!(
            beyond_right.len() >= 2,
            "expected scrolled-out cards right of the ring, got {:?}",
            beyond_right
        );
        for r in &beyond_right {
            assert!(
                r.x - CARD_INSET >= ring.x + ring.width + edge_gap - 1,
                "card {:?} too close to ring {:?} (need {}px)",
                r,
                ring,
                edge_gap
            );
        }
        for card in &row.cards {
            assert!(
                card.rect.x >= row.panel.x
                    && card.rect.x + card.rect.width <= row.panel.x + row.panel.width,
                "shifted card {:?} escapes panel {:?}",
                card.rect,
                row.panel
            );
        }

        // Scroll far right: leading columns sit left of the viewport.
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .get_mut(0)
            .unwrap()
            .set_scroll_offset(2100.0);
        let (_, model) = state.build_overview_model().expect("model");
        let row = &model.rows[0];
        let ring = row.viewport;
        let edge_gap = model.accent_width as i32 + 4;
        let beyond_left: Vec<Rect> = row
            .cards
            .iter()
            .filter(|c| c.rect.x + c.rect.width <= ring.x)
            .map(|c| c.rect)
            .collect();
        assert!(
            beyond_left.len() >= 2,
            "expected scrolled-out cards left of the ring, got {:?}",
            beyond_left
        );
        for r in &beyond_left {
            assert!(
                r.x + r.width + CARD_INSET <= ring.x - edge_gap + 1,
                "card {:?} too close to ring {:?} (need {}px)",
                r,
                ring,
                edge_gap
            );
        }
        for card in &row.cards {
            assert!(
                card.rect.x >= row.panel.x
                    && card.rect.x + card.rect.width <= row.panel.x + row.panel.width,
                "shifted card {:?} escapes panel {:?}",
                card.rect,
                row.panel
            );
        }
    }

    #[test]
    fn test_accent_width_follows_configured_border_width() {
        let mut state = test_state();
        state.config.appearance.active_border_width = 5;
        add_windows(&mut state, 0, &[101]);

        let (_, model) = state.build_overview_model().expect("model");
        assert_eq!(model.accent_width, 5, "accent width comes from config");
    }

    #[test]
    fn test_panels_wrap_content_left_aligned() {
        let mut state = test_state();
        // Row 0: a single window — content is just the viewport region.
        add_windows(&mut state, 0, &[101]);
        // Row 1: a strip much wider than the viewport.
        add_windows(&mut state, 1, &[201, 202, 203, 204]);
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .get_mut(1)
            .unwrap()
            .set_all_column_widths(1000);

        let (_, model) = state.build_overview_model().expect("model");
        let margin_x = (1920.0_f64 * OUTER_MARGIN_FRAC).round() as i32;
        let max_panel_w = 1920 - 2 * margin_x;
        let narrow = &model.rows[0];
        let wide = &model.rows[1];
        assert_eq!(narrow.panel.x, margin_x, "panels sit at the left margin");
        assert_eq!(wide.panel.x, margin_x, "panels sit at the left margin");
        assert!(
            narrow.panel.width < max_panel_w,
            "panel must wrap content, not span the full row: {} vs {}",
            narrow.panel.width,
            max_panel_w
        );
        assert!(
            narrow.panel.width < wide.panel.width,
            "panel width follows content width: {} vs {}",
            narrow.panel.width,
            wide.panel.width
        );
        for row in &model.rows {
            assert_eq!(row.label_strip.width, row.panel.width, "strip wraps too");
            // The panel hugs its content: the widest element (card edges
            // un-inset, or the raw viewport region — the model's viewport
            // is the ring rect, inflated by VIEWPORT_RING_PAD) ends one
            // body inset from the panel's right edge.
            let content_right = row
                .cards
                .iter()
                .map(|c| c.rect.x + c.rect.width + CARD_INSET)
                .chain(std::iter::once(
                    row.viewport.x + row.viewport.width - VIEWPORT_RING_PAD,
                ))
                .max()
                .unwrap();
            let panel_right = row.panel.x + row.panel.width;
            assert!(
                (panel_right - content_right - BODY_INSET).abs() <= 2,
                "panel right {} must hug content right {} + body inset",
                panel_right,
                content_right
            );
        }
    }

    #[test]
    fn test_fullscreen_workspace_yields_single_viewport_card() {
        let mut state = test_state();
        add_windows(&mut state, 0, &[101, 102, 103]);
        {
            let ws = state.workspaces.get_mut(&1).unwrap().get_mut(0).unwrap();
            assert!(ws.toggle_fullscreen(), "enter fullscreen");
        }
        let fs_wid = state.workspaces[&1][0]
            .fullscreen_window_id()
            .expect("fullscreen window");

        let (_, model) = state.build_overview_model().expect("model");
        let row = &model.rows[0];
        assert_eq!(row.cards.len(), 1, "fullscreen collapses the row to one card");
        let card = &row.cards[0];
        assert_eq!(card.window_id, fs_wid);
        // The single card shows the full viewport: work-area aspect.
        let aspect = f64::from(card.rect.width) / f64::from(card.rect.height);
        let wa_aspect = 1920.0 / 1040.0;
        assert!(
            (aspect - wa_aspect).abs() < 0.1,
            "card aspect {:.3} must match work area {:.3}",
            aspect,
            wa_aspect
        );
        assert_eq!(
            row.viewport.width, 0,
            "viewport marker hidden (it would coincide with the card)"
        );
    }

    #[test]
    fn test_overview_scaling_preserves_aspect_ratio() {
        let mut state = test_state();
        add_windows(&mut state, 0, &[101]);

        let work_area = Rect::new(0, 0, 1920, 1040);
        let placement = state.workspaces[&1][0]
            .compute_placements(work_area)
            .into_iter()
            .find(|p| p.window_id == 101)
            .expect("placement");
        let (_, model) = state.build_overview_model().expect("model");
        let card = &model.rows[0].cards[0];

        let src_aspect = f64::from(placement.rect.width) / f64::from(placement.rect.height);
        let card_aspect = f64::from(card.rect.width) / f64::from(card.rect.height);
        assert!(
            (src_aspect - card_aspect).abs() < 0.05,
            "aspect drift: source {:.3} vs card {:.3}",
            src_aspect,
            card_aspect
        );
    }

    #[test]
    fn test_app_state_skips_overview_overlay_under_cfg_test() {
        let state = test_state();
        assert!(
            state.overview_overlay.is_none(),
            "OverviewOverlay must stay None in AppState::new — it spawns a real top-level window"
        );
        assert!(!state.overview_open);
    }

    #[test]
    fn test_toggle_overview_flips_open_flag_without_overlay() {
        let mut state = test_state();
        add_windows(&mut state, 0, &[101]);
        // No event sender installed under cfg(test) -> no overlay is
        // constructed, but the open flag still tracks visibility intent.
        state.toggle_overview();
        assert!(state.overview_open);
        assert!(state.overview_overlay.is_none());
        state.toggle_overview();
        assert!(!state.overview_open);
    }

    #[test]
    fn test_toggle_overview_no_op_when_all_workspaces_empty() {
        let mut state = test_state();
        state.toggle_overview();
        assert!(!state.overview_open, "nothing to show on empty workspaces");
    }

    #[test]
    fn test_refresh_overview_model_hides_when_last_window_gone() {
        let mut state = test_state();
        add_windows(&mut state, 0, &[101]);
        state.toggle_overview();
        assert!(state.overview_open);
        state
            .workspaces
            .get_mut(&1)
            .unwrap()
            .get_mut(0)
            .unwrap()
            .remove_window(101)
            .unwrap();
        state.refresh_overview_model();
        assert!(!state.overview_open);
    }
}
