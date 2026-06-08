//! Scratchpad: one designated window that hides to a holding area and
//! re-summons floating + centered on a hotkey.
//!
//! While hidden, the window is removed from every workspace (so the
//! layout engine never touches it) and DWM-cloaked in place. While shown,
//! it lives as a floating window on whichever workspace was active when it
//! was summoned. Session-scoped: the designation is keyed by HWND and is
//! not persisted across a daemon restart.

use crate::state::{AppState, ScratchpadState};
use leopardwm_core_layout::Rect;
use tracing::{info, warn};

impl AppState {
    /// Remove `wid` from whichever workspace currently holds it (tiled or
    /// floating). Returns true if it was found and removed.
    fn detach_window_from_workspace(&mut self, wid: u64) -> bool {
        let Some((mon, ws_idx)) = self.find_window_workspace(wid) else {
            return false;
        };
        if let Some(ws) = self.workspaces.get_mut(&mon).and_then(|v| v.get_mut(ws_idx)) {
            if ws.is_floating(wid) {
                ws.remove_floating(wid);
            } else {
                let _ = ws.remove_window(wid);
            }
            return true;
        }
        false
    }

    /// A centered floating rect on the focused monitor's work area.
    pub(crate) fn centered_float_rect(&self) -> Rect {
        let wa = self.focused_viewport();
        let w = 900.min((wa.width - 80).max(200));
        let h = 600.min((wa.height - 80).max(150));
        Rect::new(wa.x + (wa.width - w) / 2, wa.y + (wa.height - h) / 2, w, h)
    }

    /// Designate the focused window as the scratchpad and hide it. If a
    /// different scratchpad is already stashed, summon it back first so it
    /// is not stranded hidden.
    pub(crate) fn scratchpad_stash(&mut self) {
        // Use the OS-foreground window, not just the focused tiled column:
        // a summoned scratchpad is a FLOATING window, which
        // `Workspace::focused_window` does not report. Without this, stashing
        // while the scratchpad is focused would grab a tiled window instead
        // and fail to recognise "release the current scratchpad".
        let Some(wid) = self
            .previous_focused_hwnd
            .or_else(|| self.focused_workspace().and_then(|ws| ws.focused_window()))
        else {
            info!("Scratchpad stash: no focused window");
            return;
        };

        // Stashing the window that is already the scratchpad releases it:
        // it returns to the tiled layout and the designation is cleared.
        // Clear the designation BEFORE releasing so the daemon never holds a
        // scratchpad pointer to an already-re-tiled window.
        if let Some(sp) = self.scratchpad {
            if sp.window_id == wid {
                self.scratchpad = None;
                self.release_to_tiling(wid, sp.origin_column);
                let _ = self.apply_layout();
                self.sync_foreground_window();
                info!("Scratchpad: released window {} back to tiling", wid);
                return;
            }
        }

        // Only stash a window that still exists; otherwise we would cloak /
        // move a dead HWND and record a dangling designation.
        #[cfg(not(test))]
        if !leopardwm_platform_win32::is_window_valid(wid) {
            info!("Scratchpad stash: focused window {} is no longer valid", wid);
            return;
        }

        // Designating a new scratchpad: release any existing one back to
        // tiling first so it is not orphaned hidden. `take()` clears the
        // designation up front, so a failure mid-release can never leave the
        // daemon pointing at a window that is already back in the layout.
        if let Some(prev) = self.scratchpad.take() {
            self.release_to_tiling(prev.window_id, prev.origin_column);
        }

        // Remember the column it currently occupies so releasing later
        // restores it to the same spot rather than one column over.
        let origin_column = self
            .focused_workspace()
            .and_then(|ws| ws.find_window_location(wid))
            .map(|(col, _)| col)
            .unwrap_or_else(|| {
                self.focused_workspace()
                    .map(|ws| ws.focused_column_index())
                    .unwrap_or(0)
            });

        // Record the designation BEFORE hiding, so if anything aborts
        // mid-hide the daemon still knows it owns this window (the destroyed
        // handler, next toggle, and shutdown/emergency recovery can all act
        // on it) rather than leaving it cloaked/off-screen with no owner.
        self.scratchpad = Some(ScratchpadState {
            window_id: wid,
            shown: false,
            origin_column,
        });
        self.hide_window_to_holding(wid);
        let _ = self.apply_layout();
        self.sync_foreground_window();
        info!("Scratchpad: stashed window {}", wid);
    }

    /// Return `wid` to the active workspace as a tiled window at
    /// `origin_column`: detach any floating entry, ensure it is uncloaked,
    /// and insert it as a column at its original index. The subsequent
    /// `apply_layout` repositions it on-screen, overriding any off-screen
    /// parking from the holding state.
    fn release_to_tiling(&mut self, wid: u64, origin_column: usize) {
        self.detach_window_from_workspace(wid);
        leopardwm_platform_win32::dwm_uncloak_window(wid);
        let reinserted = self
            .focused_workspace_mut()
            .map(|ws| ws.insert_window_at_column(wid, None, origin_column).is_ok())
            .unwrap_or(false);
        if !reinserted {
            // Reattach failed (no workspace, or a duplicate that detach
            // somehow missed). The window is uncloaked but may still be
            // parked off-screen from the holding state, so pull it back
            // on-screen rather than leave it lost.
            warn!(
                "Scratchpad: could not re-tile window {}; restoring it on-screen",
                wid
            );
            let rect = self.centered_float_rect();
            let _ = leopardwm_platform_win32::position_window(wid, rect);
        }
    }

    /// Remove `wid` from its workspace and hide it: cloak (hides from
    /// Alt-Tab/taskbar) AND park off-screen. The off-screen move is what
    /// actually removes it from view — cloaking the *foreground* window
    /// alone does not reliably hide it. Both are recovery-safe: shutdown /
    /// panic / `emergency-uncloak` drains the direct-cloak set and
    /// re-homes any off-screen window.
    fn hide_window_to_holding(&mut self, wid: u64) {
        self.detach_window_from_workspace(wid);
        leopardwm_platform_win32::dwm_cloak_window(wid);
        let _ = leopardwm_platform_win32::move_window_offscreen(wid);
    }

    /// Add `wid` as a floating, centered window on the active workspace,
    /// uncloak it, position it, and let the OS foreground event drive
    /// focus + the border. Returns `false` if the window is gone or could
    /// not be floated, so the caller can drop the designation.
    fn scratchpad_show(&mut self, wid: u64) -> bool {
        #[cfg(not(test))]
        if !leopardwm_platform_win32::is_window_valid(wid) {
            warn!("Scratchpad: cannot summon window {}; it is gone", wid);
            return false;
        }
        let rect = self.centered_float_rect();
        self.detach_window_from_workspace(wid);
        leopardwm_platform_win32::dwm_uncloak_window(wid);
        let floated = self
            .focused_workspace_mut()
            .map(|ws| {
                let ok = ws.add_floating(wid, rect).is_ok();
                if ok {
                    let _ = ws.focus_window(wid);
                }
                ok
            })
            .unwrap_or(false);
        if !floated {
            // Uncloaked but not attached to a workspace. Pull it on-screen so
            // the now-visible window is not stranded at its off-screen park.
            warn!("Scratchpad: could not float window {} on summon", wid);
            let _ = leopardwm_platform_win32::position_window(wid, rect);
            return false;
        }
        let _ = self.apply_layout();
        // Layout places floating windows asynchronously; force the final
        // position synchronously so the window is physically centered.
        let _ = leopardwm_platform_win32::position_window(wid, rect);
        // Deliberately do NOT pre-set previous_focused_hwnd here. Setting
        // the OS foreground fires EVENT_SYSTEM_FOREGROUND; the Focused
        // handler then shows the border once the window has composited at
        // its new spot (its DWM frame bounds, which the border reads, are
        // stale for a frame right after uncloak+move). Pre-setting the
        // focus would make that handler dedupe-skip and the border would
        // track the stale rect — the "no border on first summon" bug.
        #[cfg(not(test))]
        {
            let _ = leopardwm_platform_win32::set_foreground_window(wid);
        }
        true
    }

    /// Hide the currently-shown scratchpad window.
    fn scratchpad_hide(&mut self, wid: u64) {
        self.hide_window_to_holding(wid);
        let _ = self.apply_layout();
        self.sync_foreground_window();
    }

    /// Toggle scratchpad visibility (summon if hidden, hide if shown).
    pub(crate) fn scratchpad_toggle(&mut self) {
        let Some(state) = self.scratchpad else {
            info!("Scratchpad toggle: none designated");
            return;
        };
        if state.shown {
            self.scratchpad_hide(state.window_id);
            self.scratchpad = Some(ScratchpadState {
                shown: false,
                ..state
            });
            info!("Scratchpad: hid window {}", state.window_id);
        } else if self.scratchpad_show(state.window_id) {
            self.scratchpad = Some(ScratchpadState {
                shown: true,
                ..state
            });
            info!("Scratchpad: summoned window {}", state.window_id);
        } else {
            // Window vanished or could not be floated; drop the designation
            // rather than keep a dangling, un-summonable scratchpad.
            self.scratchpad = None;
            info!(
                "Scratchpad: summon of window {} failed; cleared designation",
                state.window_id
            );
        }
    }

    /// Clear the scratchpad designation if `wid` was the scratchpad
    /// (called when a window is destroyed).
    pub(crate) fn scratchpad_on_window_destroyed(&mut self, wid: u64) {
        if self.scratchpad.map(|s| s.window_id) == Some(wid) {
            self.scratchpad = None;
            info!("Scratchpad: designated window {} closed; cleared", wid);
        }
    }

    /// Re-focus the scratchpad after a workspace switch if it is shown and
    /// lives on the now-active workspace. A summoned scratchpad is a
    /// floating window on its workspace; switching away and back leaves it
    /// visible but focus lands on a tiled window, so it needs an explicit
    /// re-focus. No-op if there's no shown scratchpad on the active
    /// workspace.
    pub(crate) fn refocus_scratchpad_if_active(&mut self) {
        let Some(sp) = self.scratchpad else { return };
        if !sp.shown {
            return;
        }
        let wid = sp.window_id;
        let active = self.active_workspace_idx(self.focused_monitor);
        let on_active_workspace = self
            .workspaces
            .get(&self.focused_monitor)
            .and_then(|v| v.get(active))
            .is_some_and(|ws| ws.contains_window(wid));
        if !on_active_workspace {
            return;
        }
        if let Some(ws) = self.focused_workspace_mut() {
            let _ = ws.focus_window(wid);
        }
        self.previous_focused_hwnd = Some(wid);
        #[cfg(not(test))]
        {
            let _ = leopardwm_platform_win32::set_foreground_window(wid);
        }
    }
}
