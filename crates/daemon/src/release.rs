use crate::state::{AppState, DragHintAction};
use anyhow::{anyhow, Result};
use leopardwm_platform_win32::cascade_windows;
use std::time::Duration;

const RELEASE_BARRIER_TIMEOUT: Duration = Duration::from_millis(100);

impl AppState {
    /// Pause tiling and return every managed window to a visible cascade while
    /// retaining all workspace membership for a later resume.
    pub(crate) fn release_all_windows(&mut self) -> Result<()> {
        if !self.paused {
            self.toggle_pause("release all windows")?;
        }

        self.hide_border();
        self.hide_tab_strip();
        self.pending_drag_hint = Some(DragHintAction::Hide);
        self.previous_focused_hwnd = None;
        self.broadcast_focused_window_if_changed(self.focused_monitor as i64, None);

        self.bump_physical_invalidation();
        let (request_id, invalidation_id) = self.physical_request_ids();
        self.abandon_physical_request(request_id, invalidation_id);
        self.animation_inflight_request_id = None;
        self.applying_layout = false;
        self.abort_active_ghost_transition();
        self.abort_layout_transition();

        self.reap_finished_pending_apply_workers();
        if !self.pending_apply_workers.is_empty() {
            return Err(anyhow!(
                "An earlier placement worker is still running. No cascade was performed; tiling remains paused. Retry release-all-windows after it finishes."
            ));
        }
        if self
            .animation_worker_control
            .as_ref()
            .is_some_and(|control| !control.wait_for_barrier(RELEASE_BARRIER_TIMEOUT))
        {
            return Err(anyhow!(
                "The animation worker did not become idle in time. No cascade was performed; tiling remains paused. Retry release-all-windows after it finishes."
            ));
        }

        // The barrier retires thumbnail updates too. Release intentionally exposes
        // sources outside their tiled owners, unlike an ordinary layout landing.
        let pending_sources: Vec<_> = self
            .ghost_sources_pending_safe_landing
            .iter()
            .copied()
            .collect();
        for window_id in pending_sources {
            self.stop_ghosting_window_visuals(window_id);
        }
        let window_ids = self.all_managed_window_ids();
        self.cascade_released_windows(&window_ids)
    }

    fn cascade_released_windows(&mut self, window_ids: &[u64]) -> Result<()> {
        #[cfg(test)]
        if let Some(result) = &self.injected_release_cascade_result {
            self.released_window_id_batches.push(window_ids.to_vec());
            return result.clone().map_err(anyhow::Error::msg);
        }
        cascade_windows(window_ids).map_err(Into::into)
    }
}
