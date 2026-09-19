use crate::state::{AppState, DragHintAction};
#[cfg(test)]
use anyhow::anyhow;
use anyhow::Result;
#[cfg(not(test))]
use leopardwm_platform_win32::cascade_windows;

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

        let window_ids = self.all_managed_window_ids();
        self.cascade_released_windows(&window_ids)
    }

    #[cfg(not(test))]
    fn cascade_released_windows(&mut self, window_ids: &[u64]) -> Result<()> {
        cascade_windows(window_ids).map_err(Into::into)
    }

    #[cfg(test)]
    fn cascade_released_windows(&mut self, window_ids: &[u64]) -> Result<()> {
        self.released_window_id_batches.push(window_ids.to_vec());
        match &self.injected_release_cascade_error {
            Some(error) => Err(anyhow!(error.clone())),
            None => Ok(()),
        }
    }
}
