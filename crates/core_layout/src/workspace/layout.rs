use crate::*;

use crate::workspace::Workspace;

impl Workspace {
    /// Compute placements for all windows given a viewport.
    ///
    /// Returns a list of WindowPlacement structs indicating where each window
    /// should be positioned and whether it's visible or off-screen.
    ///
    /// Note: Negative gaps are treated as zero for calculation purposes.
    pub fn compute_placements(&self, viewport: Rect) -> Vec<WindowPlacement> {
        // Use rounding instead of truncation to prevent sub-pixel jitter
        let viewport_left = self.scroll_offset.round() as i32;

        // Fullscreen mode: one window covers the entire viewport, others are off-screen
        if let Some(fs_wid) = self.fullscreen_window {
            return self.compute_fullscreen_placements(fs_wid, viewport, viewport_left);
        }

        self.compute_non_fullscreen_placements(viewport, viewport_left)
    }

    /// Compute placements for the ENTIRE strip, ignoring scroll: viewport
    /// left is pinned to 0 and the caller supplies a viewport wide enough
    /// for every column, so nothing is marked off-screen. Used by the
    /// workspace overview to miniaturize the whole strip.
    pub fn placements_for_full_strip(&self, viewport: Rect) -> Vec<WindowPlacement> {
        self.compute_non_fullscreen_placements(viewport, 0)
    }

    /// Compute non-fullscreen placements for a specific viewport-left offset.
    /// Used by both static and animated placement paths.
    fn compute_non_fullscreen_placements(
        &self,
        viewport: Rect,
        viewport_left: i32,
    ) -> Vec<WindowPlacement> {
        let mut placements = Vec::new();

        // Defensively clamp gaps to >= 0 in case fields were set directly
        let gap = self.gap.max(0);
        let outer_left = self.outer_gap_left.max(0);
        let outer_top = self.outer_gap_top.max(0);
        let outer_bottom = self.outer_gap_bottom.max(0);

        // Visible strip area inside viewport padding
        let vis_w = self.visible_width(viewport.width);
        let visible_right = viewport_left.saturating_add(vis_w);

        // Pre-compute effective column widths respecting window min-widths.
        // If a column contains a window with a known minimum width larger than
        // the allocated column width, widen it and shrink flexible columns.
        let effective_widths: Vec<i32> = {
            let mut widths: Vec<i32> = self.columns.iter().map(|c| c.width).collect();
            if !self.window_min_widths.is_empty() {
                let mut excess = 0i32;
                let mut flexible_total = 0i32;
                for (col_idx, column) in self.columns.iter().enumerate() {
                    if !self.is_column_active(column) {
                        continue;
                    }
                    let min_w = self.column_effective_min_width(column);
                    if min_w > column.width {
                        excess += min_w - column.width;
                        widths[col_idx] = min_w;
                    } else {
                        flexible_total += column.width;
                    }
                }
                if excess > 0 && flexible_total > 0 {
                    let mut remaining = excess;
                    for (col_idx, column) in self.columns.iter().enumerate() {
                        if !self.is_column_active(column) || widths[col_idx] != column.width {
                            continue;
                        }
                        let share = ((column.width as f64 / flexible_total as f64) * excess as f64)
                            .round() as i32;
                        let shrink = share.min(remaining).min(column.width - MIN_COLUMN_WIDTH).max(0);
                        widths[col_idx] -= shrink;
                        remaining -= shrink;
                    }
                }
            }
            widths
        };

        // Strip starts at 0 — outer gaps are viewport padding
        let mut current_x: i32 = 0;

        for (col_idx, column) in self.columns.iter().enumerate() {
            let eff_width = effective_widths[col_idx];

            // Calculate column position in strip coordinates
            let col_strip_x = current_x;
            let col_strip_right = col_strip_x.saturating_add(eff_width);

            // Transform to screen coordinates:
            // strip_x → screen_x = strip_x - scroll_offset + viewport.x + outer_left
            // Must use regular arithmetic (not saturating) to allow negative screen
            // positions for columns partially scrolled off the left edge.
            let natural_screen_x = col_strip_x - viewport_left + viewport.x + outer_left;

            // Determine visibility against the visible strip area
            let visibility = if col_strip_right <= viewport_left {
                Visibility::OffScreenLeft
            } else if col_strip_x >= visible_right {
                Visibility::OffScreenRight
            } else {
                Visibility::Visible
            };

            // Push off-screen columns past the viewport edge so they cannot
            // peek through the outer-gap area when DWM cloaking is slow or
            // ineffective. The natural strip-flow position for an
            // `OffScreenRight` column at the boundary is
            // `viewport.x + viewport.width - outer_right` — i.e. INSIDE
            // the viewport by `outer_right` pixels — so the leftmost
            // off-screen-right column's first `outer_right` pixels would
            // otherwise be visible if anything fails to cloak it.
            // Symmetric clamp for `OffScreenLeft`.
            //
            // The clamp/no-clamp boundary aligns with the cloak/uncloak
            // boundary in `apply_placements`: a column transitions from
            // OffScreen to Visible in the same `apply` call where the
            // window is uncloaked, so the position jump is invisible to
            // the user. Animation frames continue to use this placement
            // logic so the position transitions atomically with the
            // visibility flip.
            let col_screen_x = match visibility {
                Visibility::OffScreenRight => natural_screen_x
                    .max(viewport.x.saturating_add(viewport.width)),
                Visibility::OffScreenLeft => natural_screen_x
                    .min(viewport.x.saturating_sub(eff_width)),
                Visibility::Visible => natural_screen_x,
            };

            // Build the set of windows that occupy column geometry on this pass.
            // Vertical: all non-minimized windows split by height_weights.
            // Tabbed: only the active tab takes the full column rect; if it's
            // minimized, fall back to the first visible tab so the column
            // doesn't render empty.
            let visible_windows: Vec<(usize, WindowId)> = match column.mode() {
                crate::ColumnMode::Vertical => column
                    .windows()
                    .iter()
                    .enumerate()
                    .filter(|(_, w)| !self.minimized_windows.contains(w))
                    .map(|(i, &w)| (i, w))
                    .collect(),
                crate::ColumnMode::Tabbed { .. } => {
                    // Shared picker: prefer active tab, fall back to first
                    // visible if active is minimized.
                    column
                        .effective_visible_tab(|w| self.minimized_windows.contains(&w))
                        .and_then(|i| column.windows().get(i).map(|&w| (i, w)))
                        .into_iter()
                        .collect()
                }
            };

            // Skip columns where all windows are minimized
            if !self.is_column_active(column) {
                continue;
            }

            // In Tabbed mode, every non-active non-minimized tab gets an
            // off-screen placement so the daemon's cloak machinery hides it.
            // (Minimized tabs are already excluded by the apply path.)
            //
            // The rect is positioned `viewport.width` pixels to the LEFT of
            // the viewport so the window is genuinely off-screen even with
            // `SWP_NOSIZE` keeping its previous size. Cloak is still applied
            // by the platform layer; this position is defense-in-depth in
            // case cloak races, fails, or hasn't taken effect on the first
            // frame after the toggle.
            if column.is_tabbed() {
                let on_screen_idx = visible_windows.first().map(|(i, _)| *i);
                let offscreen_x = viewport.x.saturating_sub(viewport.width.max(1));
                for (i, &wid) in column.windows().iter().enumerate() {
                    if Some(i) == on_screen_idx {
                        continue;
                    }
                    if self.minimized_windows.contains(&wid) {
                        continue;
                    }
                    placements.push(WindowPlacement {
                        window_id: wid,
                        rect: Rect::new(offscreen_x, viewport.y, 0, 0),
                        visibility: Visibility::OffScreenLeft,
                        column_index: col_idx,
                    });
                }
            }

            // Reserve space at the top of Tabbed columns for the tab strip
            // overlay. Without this the strip is positioned at
            // `column.y - strip_h`, which lands above the work-area top
            // edge when `outer_top` is small — i.e. invisible. Reserving
            // shifts the active tab down by `strip_h` so the overlay has
            // room to render *inside* the column's allocated area.
            let column_top_reserve = if column.is_tabbed() {
                self.tab_strip_reserve_px.max(0)
            } else {
                0
            };

            // Build visible-window weights
            let usable_height = viewport
                .height
                .saturating_sub(outer_top)
                .saturating_sub(outer_bottom)
                .saturating_sub(column_top_reserve)
                .max(0);
            let window_count = visible_windows.len() as i32;
            let window_gaps = if window_count > 1 {
                gap.saturating_mul(window_count - 1)
            } else {
                0
            };
            let available_height = (usable_height - window_gaps).max(0);

            // Compute per-window heights respecting known min-heights. Each
            // window with a recorded minimum is pinned to at least that
            // minimum; the remaining space is distributed among the flexible
            // (no-min) windows using their height weights. The last window
            // absorbs rounding remainder so the column stays flush with the
            // viewport — this also means if there are no flexible windows,
            // any leftover space simply flows into the last pinned window.
            let visible_weights: Vec<f64> = if column.height_weights.len() == column.windows().len() {
                visible_windows.iter().map(|(i, _)| column.height_weights[*i]).collect()
            } else {
                vec![1.0; visible_windows.len()]
            };

            let min_heights: Vec<i32> = visible_windows.iter()
                .map(|(_, wid)| self.window_min_heights.get(wid).copied().unwrap_or(0))
                .collect();
            let total_min: i32 = min_heights.iter().sum();
            let flex_height = (available_height - total_min).max(0);

            // Sum of weights over windows that don't have a pinned minimum.
            let flex_weight_sum: f64 = visible_weights
                .iter()
                .zip(min_heights.iter())
                .filter(|(_, m)| **m == 0)
                .map(|(w, _)| *w)
                .sum();
            // If any flexible window exists, pinned windows get exactly their
            // minimum and flexible windows share flex_height. If every window
            // is pinned, pinned windows still get exactly their minimum and
            // the last-window remainder rule absorbs any leftover space.
            let has_flex = flex_weight_sum > 0.0;

            let mut current_y = viewport.y + outer_top + column_top_reserve;

            for (win_idx, &(_, window_id)) in visible_windows.iter().enumerate() {
                let is_last = win_idx == visible_windows.len() - 1;
                let height = if is_last {
                    // Last window absorbs the rounding remainder so the column
                    // stays flush with the viewport, but we honor its minimum
                    // even if doing so causes the column to overflow downward
                    // (the alternative — silently violating its minimum — is
                    // what Slack/Spotify did before this fix and the whole
                    // point of the contract is to never let that happen).
                    let remainder =
                        (viewport.y + viewport.height - outer_bottom - current_y).max(0);
                    remainder.max(min_heights[win_idx])
                } else if min_heights[win_idx] > 0 {
                    // Pinned non-last window: exactly its minimum.
                    min_heights[win_idx]
                } else if has_flex {
                    // Flexible window: share of flex_height by weight.
                    let share = visible_weights[win_idx] / flex_weight_sum;
                    (flex_height as f64 * share).round() as i32
                } else {
                    // No flex windows, and this one isn't pinned — give it an
                    // even split of available_height as a last resort.
                    available_height / visible_windows.len().max(1) as i32
                };

                placements.push(WindowPlacement {
                    window_id,
                    rect: Rect::new(col_screen_x, current_y, eff_width, height),
                    visibility,
                    column_index: col_idx,
                });

                current_y = current_y.saturating_add(height).saturating_add(gap);
            }

            current_x = current_x.saturating_add(eff_width).saturating_add(gap);
        }

        // Add floating windows (visible unless minimized, at their absolute positions)
        for floating in &self.floating_windows {
            if self.minimized_windows.contains(&floating.id) {
                continue;
            }
            placements.push(WindowPlacement {
                window_id: floating.id,
                rect: floating.rect,
                visibility: Visibility::Visible,
                column_index: usize::MAX, // Sentinel for floating windows
            });
        }

        placements
    }

    /// Compute placements for all windows, using animated scroll offset if active.
    ///
    /// This is similar to `compute_placements` but uses `effective_scroll_offset()`
    /// to support smooth scrolling animations.
    pub fn compute_placements_animated(&self, viewport: Rect) -> Vec<WindowPlacement> {
        // Use animated scroll offset
        let viewport_left = self.effective_scroll_offset().round() as i32;

        // Fullscreen mode: one window covers the entire viewport, others are off-screen
        if let Some(fs_wid) = self.fullscreen_window {
            return self.compute_fullscreen_placements(fs_wid, viewport, viewport_left);
        }

        self.compute_non_fullscreen_placements(viewport, viewport_left)
    }

    /// Compute placements when a window is fullscreen.
    ///
    /// The fullscreen window gets the full viewport; all others are marked
    /// off-screen but KEEP their real layout rects — cloaking hides them, and
    /// the platform layer moves off-screen windows with `SWP_NOSIZE`, so a
    /// zeroed rect would visibly snap them to the top-left corner. Pinned
    /// floating windows stay Visible at their floating rect, above fullscreen.
    fn compute_fullscreen_placements(
        &self,
        fs_wid: WindowId,
        viewport: Rect,
        viewport_left: i32,
    ) -> Vec<WindowPlacement> {
        // Stale or minimized fullscreen target: fall back to normal placements.
        if !self.contains_window(fs_wid) || self.minimized_windows.contains(&fs_wid) {
            return self.compute_non_fullscreen_placements(viewport, viewport_left);
        }

        let mut placements = self.compute_non_fullscreen_placements(viewport, viewport_left);

        for placement in &mut placements {
            if placement.window_id == fs_wid {
                placement.rect = viewport;
                placement.visibility = Visibility::Visible;
            } else if placement.column_index == usize::MAX
                && self
                    .floating_windows
                    .iter()
                    .any(|f| f.id == placement.window_id && f.pinned)
            {
                // Pinned floating window: stays exactly as computed.
            } else {
                placement.visibility = Visibility::OffScreenLeft;
            }
        }

        placements
    }
}
