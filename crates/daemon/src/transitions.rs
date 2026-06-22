//! Animation transitions: layout/workspace-switch transitions, the ghost-thumbnail path, and motion settings.

use crate::animation_worker;
use crate::state::*;
use std::collections::HashMap;
use tracing::info;

impl AppState {
    /// Check if any workspace has an active animation or layout transition.
    pub(crate) fn is_animating(&self) -> bool {
        self.layout_transition.is_some()
            || self.workspaces.values().any(|ws_vec| ws_vec.iter().any(|w| w.is_animating()))
    }

    /// Tick all active animations by the given delta time.
    /// Returns true if any animation is still running.
    pub(crate) fn tick_animations(&mut self, delta_ms: u64) -> bool {
        let mut still_animating = false;
        let mut scroll_anims_settled = false;
        for ws_vec in self.workspaces.values_mut() {
            for workspace in ws_vec.iter_mut() {
                let was_animating = workspace.is_animating();
                if workspace.tick_animation(delta_ms) {
                    still_animating = true;
                } else if was_animating {
                    // A scroll animation just finished: scroll_offset is now the
                    // target, so viewport visibility (and taskbar buttons) can be
                    // reconciled against the settled position. An animated focus
                    // scroll leaves scroll_offset stale until exactly this point.
                    scroll_anims_settled = true;
                }
            }
        }
        if scroll_anims_settled {
            self.sync_taskbar_buttons();
        }
        if let Some(ref mut transition) = self.layout_transition {
            if transition.tick(delta_ms) {
                still_animating = true;
            } else {
                // Transition complete — move exiting windows offscreen.
                for wid in transition.exit_rects.keys() {
                    let _ = leopardwm_platform_win32::move_window_offscreen(*wid);
                }
                self.layout_transition = None;
                // The slide is done; cloak any settled off-workspace windows
                // that were skipped while animating so their taskbar buttons go.
                self.sync_taskbar_buttons();
                // Signal one more frame so entering windows land at their
                // exact final positions (previous frame had t < 1.0).
                still_animating = true;
            }
        }
        still_animating
    }

    /// Snapshot the current placement rects for all tiled windows.
    /// Call this *before* a structural layout change.
    pub(crate) fn snapshot_layout(&self) -> std::collections::HashMap<u64, leopardwm_core_layout::Rect> {
        let mut rects = std::collections::HashMap::new();
        for (monitor_id, ws_vec) in &self.workspaces {
            let idx = self.active_workspace_idx(*monitor_id);
            if let Some(workspace) = ws_vec.get(idx) {
                if self.monitors.contains_key(monitor_id) {
                    let viewport = self.layout_viewport(*monitor_id);
                    for p in workspace.compute_placements_animated(viewport) {
                        rects.insert(p.window_id, p.rect);
                    }
                }
            }
        }
        rects
    }

    /// Start a layout transition animation from a pre-change snapshot.
    /// Call this *after* the structural change and ensure_focused_visible_animated.
    /// No-op when reduce_motion is active.
    pub(crate) fn start_layout_transition(
        &mut self,
        start_rects: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
    ) {
        if self.reduce_motion {
            return;
        }
        let duration = self.config.animation.layout_duration_ms;
        self.start_layout_transition_with_duration(start_rects, duration);
    }

    pub(crate) fn start_layout_transition_with_duration(
        &mut self,
        start_rects: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
        duration_ms: u64,
    ) {
        // Any prior ghost transition or in-flight crossfade is invalidated
        // by a new transition starting. Drops handles via Drop, uncloaks
        // sources, and tells the worker to abort the fade.
        self.abort_active_ghost_transition();

        // Determine which (if any) windows in this transition are eligible
        // for the ghost-animation path. Targets are evaluated against the
        // structural placements that will be live after this transition.
        let mut ghosted_wids = std::collections::HashSet::new();
        if self.config.behavior.swap_chain_ghost_animation {
            self.register_ghosts_for_transition(&start_rects, &mut ghosted_wids);
        }

        // Start with one frame (~16ms) already elapsed so the first
        // apply_layout/send_animation_frame shows visible movement.
        self.layout_transition = Some(LayoutTransition {
            start_rects,
            exit_rects: HashMap::new(),
            elapsed_ms: 16,
            duration_ms,
            easing: self.config.animation.easing,
            ghosted_wids,
        });
    }

    /// Start a workspace switch transition that animates both entering and
    /// exiting windows simultaneously (continuous vertical scroll effect).
    /// No-op when reduce_motion is active.
    ///
    /// Workspace-switch transitions never use the ghost path: every window
    /// either slides off-screen (exit_rects) or slides in from off-screen,
    /// neither of which is the rapid-async-burst-while-visible scenario
    /// that the swap-chain bug exhibits.
    pub(crate) fn start_workspace_switch_transition(
        &mut self,
        start_rects: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
        exit_rects: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
        duration_ms: u64,
    ) {
        if self.reduce_motion {
            return;
        }
        self.abort_active_ghost_transition();
        self.layout_transition = Some(LayoutTransition {
            start_rects,
            exit_rects,
            elapsed_ms: 16,
            duration_ms,
            easing: self.config.animation.easing,
            ghosted_wids: std::collections::HashSet::new(),
        });
    }

    /// Drop any in-flight ghost-animation handles and uncloak their
    /// sources, then signal the worker to abort any running crossfade.
    ///
    /// Routed through by every code path that mutates or clears
    /// `layout_transition`. No-op when no ghost state is alive.
    pub(crate) fn abort_active_ghost_transition(&mut self) {
        // Phase 1: drop ghost_handles. Each GhostEntry::Drop calls
        // thumbnail::unregister_raw — no manual cleanup needed.
        let wids: Vec<u64> = self.ghost_handles.keys().copied().collect();
        self.ghost_handles.clear();

        // Phase 2: uncloak the (formerly) ghosted sources. Routes through
        // apply_cloak_state so a window also in GLOBAL_CLOAKED (off-screen
        // parked) stays cloaked.
        for wid in &wids {
            leopardwm_platform_win32::unmark_ghost_cloaked(*wid);
            leopardwm_platform_win32::apply_cloak_state(*wid);
        }

        // Phase 3: clear ghosted_wids on any still-live transition so
        // partition_for_animation no longer routes frames for them.
        if let Some(ref mut transition) = self.layout_transition {
            transition.ghosted_wids.clear();
        }

        // Phase 4: signal any in-flight crossfade to abort. Worker will
        // ack via DaemonEvent::CrossfadeComplete { epoch }; that epoch's
        // entry in crossfade_sources is removed then.
        self.abort_active_crossfade();
    }

    /// Send `AbortCrossfade { epoch }` to the worker if a fade is in
    /// flight. The worker checks between fade iterations and exits
    /// early; CrossfadeComplete arrives within ~16ms (one DwmFlush).
    ///
    /// Daemon-side `active_crossfade` is cleared immediately so any
    /// subsequent `should_ghost` evaluation doesn't see stale state,
    /// but `crossfade_sources` stays populated until CrossfadeComplete
    /// confirms the worker has stopped using the entries. This avoids
    /// re-registering a thumbnail for the same source HWND while the
    /// worker may still be updating the old one (Microsoft Q&A 3229922).
    pub(crate) fn abort_active_crossfade(&mut self) {
        if let Some(state) = self.active_crossfade.take() {
            if let Some(ref ctrl) = self.animation_worker_control {
                ctrl.send_abort_crossfade(state.epoch);
            }
            // crossfade_sources[epoch] intentionally left populated until
            // the worker acks via CrossfadeComplete. The main-loop handler
            // removes that epoch's entry then. Per-epoch tracking is what
            // makes this safe under overlapping aborts.
        }
    }

    /// Drop re-registration barriers whose `CrossfadeComplete` never
    /// arrived (worker died/stuck), so their source wids aren't stranded
    /// out of the ghost path forever. A crossfade can't legitimately
    /// outlive `CROSSFADE_BARRIER_MAX_AGE`. Run at the top of every ghost
    /// registration pass.
    pub(crate) fn sweep_stale_crossfade_barriers(&mut self) {
        self.crossfade_sources
            .retain(|_, (_, at)| at.elapsed() < crate::state::CROSSFADE_BARRIER_MAX_AGE);
    }

    /// Walk the placements that will be live after the structural change,
    /// register thumbnails for swap-chain-sensitive windows whose rect
    /// is changing, and cloak the sources via GHOST_CLOAKED. Populates
    /// `ghosted_wids` with the WindowIds we successfully registered.
    fn register_ghosts_for_transition(
        &mut self,
        start_rects: &std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
        ghosted_wids: &mut std::collections::HashSet<u64>,
    ) {
        self.sweep_stale_crossfade_barriers();

        if !leopardwm_platform_win32::thumbnail::host().is_available() {
            return;
        }
        let host_origin = leopardwm_platform_win32::thumbnail::host().origin();
        let focused = self.previous_focused_hwnd;

        // Build a (wid -> (target_rect, monitor_id)) map from the
        // animated placements on each monitor's active workspace.
        let mut targets: std::collections::HashMap<
            u64,
            (leopardwm_core_layout::Rect, leopardwm_platform_win32::MonitorId),
        > = std::collections::HashMap::new();
        for (monitor_id, ws_vec) in &self.workspaces {
            let idx = self.active_workspace_idx(*monitor_id);
            if let Some(workspace) = ws_vec.get(idx) {
                if self.monitors.contains_key(monitor_id) {
                    let viewport = self.layout_viewport(*monitor_id);
                    for p in workspace.compute_placements_animated(viewport) {
                        if p.visibility == leopardwm_core_layout::Visibility::Visible {
                            targets.insert(p.window_id, (p.rect, *monitor_id));
                        }
                    }
                }
            }
        }

        // Identify which monitor the focused window is on (used to gate
        // cross-monitor moves out of the ghost path).
        let focused_monitor = focused
            .and_then(|wid| targets.get(&wid).map(|(_, mon)| *mon))
            .unwrap_or(self.focused_monitor);

        for (&wid, &start_rect) in start_rects {
            let Some(&(target_rect, monitor_id)) = targets.get(&wid) else {
                continue;
            };
            if start_rect == target_rect {
                continue;
            }
            // Cross-monitor moves use the legacy nudge path — the
            // thumbnail host covers the virtual screen but cross-monitor
            // animation is rare (drag-only) and rarely hits the
            // rapid-async-burst case.
            if monitor_id != focused_monitor {
                continue;
            }
            // Skip the focused window: SetForegroundWindow on a cloaked
            // HWND is undocumented behavior. The focused window still
            // gets the (w-1 → w) nudge at landing.
            if focused == Some(wid) {
                continue;
            }
            // Same-source re-registration barrier (Microsoft Q&A 3229922):
            // refuse if ANY pending-ack crossfade epoch still owns this wid.
            if self
                .crossfade_sources
                .values()
                .any(|(set, _)| set.contains(&wid))
            {
                continue;
            }
            let class = leopardwm_platform_win32::thumbnail::class_name(wid);
            if !leopardwm_platform_win32::thumbnail::is_swap_chain_class_str(&class) {
                continue;
            }
            match leopardwm_platform_win32::thumbnail::register(wid) {
                Ok(handle) => {
                    let final_dest = leopardwm_platform_win32::thumbnail::screen_to_host_client(
                        target_rect,
                        host_origin,
                    );
                    let entry = crate::state::GhostEntry::new(
                        handle.into_isize(),
                        class,
                        final_dest,
                    );
                    self.ghost_handles.insert(wid, entry);
                    leopardwm_platform_win32::mark_ghost_cloaked(wid);
                    leopardwm_platform_win32::apply_cloak_state(wid);
                    ghosted_wids.insert(wid);
                }
                Err(e) => {
                    tracing::warn!("ghost register failed for {wid}: {e}");
                }
            }
        }
        if !ghosted_wids.is_empty() {
            tracing::debug!(
                "ghost: registered {} thumbnail(s), balance={}",
                ghosted_wids.len(),
                leopardwm_platform_win32::thumbnail::current_register_balance()
            );
        }
    }

    /// Apply layout transition interpolation to placements, including exit windows.
    pub(crate) fn apply_transition_interpolation(
        transition: &LayoutTransition,
        placements: &mut Vec<leopardwm_core_layout::WindowPlacement>,
    ) {
        let t = transition.eased_progress();
        // Interpolate entering/morphing windows.
        for p in placements.iter_mut() {
            if let Some(start) = transition.start_rects.get(&p.window_id) {
                p.rect = leopardwm_core_layout::Rect::new(
                    start.x + ((p.rect.x - start.x) as f64 * t).round() as i32,
                    start.y + ((p.rect.y - start.y) as f64 * t).round() as i32,
                    start.width + ((p.rect.width - start.width) as f64 * t).round() as i32,
                    start.height + ((p.rect.height - start.height) as f64 * t).round() as i32,
                );
            }
        }
        // Interpolate exiting windows (e.g., old workspace sliding out).
        for (wid, target) in &transition.exit_rects {
            if let Some(start) = transition.start_rects.get(wid) {
                placements.push(leopardwm_core_layout::WindowPlacement {
                    window_id: *wid,
                    rect: leopardwm_core_layout::Rect::new(
                        start.x + ((target.x - start.x) as f64 * t).round() as i32,
                        start.y + ((target.y - start.y) as f64 * t).round() as i32,
                        start.width + ((target.width - start.width) as f64 * t).round() as i32,
                        start.height
                            + ((target.height - start.height) as f64 * t).round() as i32,
                    ),
                    visibility: leopardwm_core_layout::Visibility::Visible,
                    column_index: 0,
                });
            }
        }
    }

    /// Split animated placements into (live, ghost) streams. Ghosted wids
    /// — those in `LayoutTransition.ghosted_wids` with a matching entry in
    /// `ghost_handles` — get a `GhostFrame` per frame instead of a per-
    /// frame SetWindowPos on the live HWND.
    ///
    /// Pure function: no Win32 calls, no mutation. Unit-testable with
    /// stub `GhostEntry` values.
    pub(crate) fn partition_for_animation(
        placements: Vec<leopardwm_core_layout::WindowPlacement>,
        transition: Option<&LayoutTransition>,
        ghost_handles: &std::collections::HashMap<u64, crate::state::GhostEntry>,
    ) -> (
        Vec<leopardwm_core_layout::WindowPlacement>,
        Vec<animation_worker::GhostFrame>,
    ) {
        let mut live: Vec<leopardwm_core_layout::WindowPlacement> =
            Vec::with_capacity(placements.len());
        let mut ghosts: Vec<animation_worker::GhostFrame> = Vec::new();

        let host_origin = leopardwm_platform_win32::thumbnail::host().origin();
        let ghosted_wids = transition.map(|t| &t.ghosted_wids);

        for p in placements {
            let is_ghost = ghosted_wids
                .map(|set| set.contains(&p.window_id))
                .unwrap_or(false);
            if is_ghost {
                if let Some(entry) = ghost_handles.get(&p.window_id) {
                    let dest = leopardwm_platform_win32::thumbnail::screen_to_host_client(
                        p.rect,
                        host_origin,
                    );
                    ghosts.push(animation_worker::GhostFrame {
                        handle_isize: entry.handle(),
                        dest_client_rect: dest,
                        opacity: 255,
                        visible: true,
                    });
                }
                // If transition has the wid but ghost_handles doesn't —
                // e.g., registration failed earlier — drop the placement
                // entirely. The window will land at its target rect via
                // the post-animation landing pass.
            } else {
                live.push(p);
            }
        }

        (live, ghosts)
    }

    /// Recompute `reduce_motion` from the accessibility setting and power state,
    /// propagating to all workspaces when the value changes.
    pub(crate) fn refresh_reduce_motion(&mut self) {
        let should_reduce =
            !leopardwm_platform_win32::are_animations_enabled() || self.on_battery_or_saver;
        if should_reduce != self.reduce_motion {
            self.reduce_motion = should_reduce;
            for ws_vec in self.workspaces.values_mut() {
                for ws in ws_vec.iter_mut() {
                    ws.set_reduce_motion(should_reduce);
                }
            }
            info!(
                "Animations {}",
                if should_reduce { "disabled" } else { "enabled" }
            );
        }
    }
}
