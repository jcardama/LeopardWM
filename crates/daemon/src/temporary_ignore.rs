//! Session-only temporary ignore for the actual OS foreground window.

use crate::event_handler::{AdmissionKind, AdmitOutcome};
use crate::state::AppState;
use leopardwm_ipc::IpcResponse;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TemporaryIgnoreEntry {
    pub token: u64,
    pub process_id: u32,
    pub class_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IgnoreGate {
    Allow,
    Block,
}

impl AppState {
    pub(crate) fn toggle_ignore(&mut self) -> IpcResponse {
        let hwnd = match self.actual_foreground_hwnd() {
            Ok(hwnd) => hwnd,
            Err(message) => return IpcResponse::error(message),
        };
        if self.find_window_workspace(hwnd).is_some() {
            self.temporarily_unmanage(hwnd)
        } else {
            self.readmit_temporarily_ignored(hwnd)
        }
    }

    fn actual_foreground_hwnd(&mut self) -> Result<u64, String> {
        match self.departing_foreground_evidence() {
            Some((Some(hwnd), true)) => Ok(hwnd),
            Some((Some(_), false)) => Err("Foreground window is not valid".into()),
            _ => Err("No foreground window".into()),
        }
    }

    pub(crate) fn temporary_ignore_gate(&mut self, hwnd: u64) -> IgnoreGate {
        let Some(entry) = self.temporary_ignores.get(&hwnd).cloned() else {
            return IgnoreGate::Allow;
        };
        match self.read_ignore_identity(hwnd) {
            Ok(Some(token)) if token == entry.token => {
                debug!(
                    "Temporary ignore still live for {} (pid {} class {})",
                    hwnd, entry.process_id, entry.class_name
                );
                IgnoreGate::Block
            }
            Ok(Some(_)) | Ok(None) => {
                if self
                    .temporary_ignores
                    .get(&hwnd)
                    .is_some_and(|current| current.token == entry.token)
                {
                    self.temporary_ignores.remove(&hwnd);
                }
                IgnoreGate::Allow
            }
            Err(error) => {
                debug!(
                    "Temporary ignore identity read failed for {}: {}; keeping ignore",
                    hwnd, error
                );
                IgnoreGate::Block
            }
        }
    }

    pub(crate) fn on_temporary_ignore_destroyed(&mut self, hwnd: u64) {
        let Some(entry) = self.temporary_ignores.get(&hwnd).cloned() else {
            return;
        };
        match self.read_ignore_identity(hwnd) {
            Ok(Some(token)) if token == entry.token => {
                debug!(
                    "Keeping temporary ignore for live hwnd {} (pid {} class {})",
                    hwnd, entry.process_id, entry.class_name
                );
            }
            Ok(Some(_)) | Ok(None) => {
                if self
                    .temporary_ignores
                    .get(&hwnd)
                    .is_some_and(|current| current.token == entry.token)
                {
                    self.temporary_ignores.remove(&hwnd);
                }
            }
            Err(error) => {
                debug!(
                    "Keeping temporary ignore for {} after destroy identity failure: {}",
                    hwnd, error
                );
            }
        }
    }

    fn temporarily_unmanage(&mut self, hwnd: u64) -> IpcResponse {
        let token = match self.stamp_ignore_identity(hwnd) {
            Ok(token) => token,
            Err(error) => {
                return IpcResponse::error(format!("Failed to stamp window identity: {error}"));
            }
        };
        if let Err(reason) = self.drain_pending_placement_work() {
            let _ = self.clear_ignore_identity(hwnd);
            return IpcResponse::error(format!("Window remains managed: {reason}"));
        }
        match self.read_ignore_identity(hwnd) {
            Ok(Some(live)) if live == token => {}
            Ok(_) => {
                self.remove_managed_membership(hwnd);
                self.forget_managed_metadata(hwnd);
                return IpcResponse::error(
                    "Foreground window is no longer the stamped lifetime and was not ignored"
                        .to_string(),
                );
            }
            Err(error) => {
                return IpcResponse::error(format!("Window remains managed: {error}"));
            }
        }

        let snapshot = self.snapshot_layout();
        let was_tiled = self.remove_managed_membership(hwnd);
        let restore_result = self.restore_unmanaged_native_state(hwnd);
        let (process_id, class_name) = self
            .lookup_window_info(hwnd)
            .map(|info| (info.process_id, info.class_name))
            .unwrap_or((0, String::new()));
        self.temporary_ignores.insert(
            hwnd,
            TemporaryIgnoreEntry {
                token,
                process_id,
                class_name: class_name.clone(),
            },
        );
        if was_tiled {
            let mut snapshot = snapshot;
            snapshot.remove(&hwnd);
            if self.start_layout_transition(snapshot) {
                if let Some(ref mut transition) = self.layout_transition {
                    transition.suppress_landing_focus_resync = true;
                }
            }
        }
        if let Err(error) = self.apply_layout() {
            warn!("Failed to apply layout after toggle-ignore: {}", error);
        }
        match restore_result {
            Ok(()) => {
                info!(
                    "Temporarily ignored window {} (pid {} class {})",
                    hwnd, process_id, class_name
                );
                IpcResponse::Ok
            }
            Err(error) => IpcResponse::error(format!(
                "Window was unmanaged but native restore failed: {error}"
            )),
        }
    }

    fn readmit_temporarily_ignored(&mut self, hwnd: u64) -> IpcResponse {
        let Some(entry) = self.temporary_ignores.get(&hwnd).cloned() else {
            return IpcResponse::error(
                "Foreground window is not managed and is not temporarily ignored",
            );
        };
        match self.read_ignore_identity(hwnd) {
            Ok(Some(token)) if token == entry.token => {}
            Ok(Some(_)) | Ok(None) => {
                if self
                    .temporary_ignores
                    .get(&hwnd)
                    .is_some_and(|current| current.token == entry.token)
                {
                    self.temporary_ignores.remove(&hwnd);
                }
                return IpcResponse::error(
                    "Foreground window is not the ignored lifetime and was not re-admitted",
                );
            }
            Err(error) => {
                return IpcResponse::error(format!("Window remains ignored: {error}"));
            }
        }
        if let Err(reason) = self.drain_pending_placement_work() {
            return IpcResponse::error(format!("Window remains ignored: {reason}"));
        }
        let outcome = self.try_admit_window(hwnd, AdmissionKind::ExplicitReadmit);
        if matches!(
            outcome,
            AdmitOutcome::Admitted | AdmitOutcome::AlreadyManaged
        ) {
            self.temporary_ignores.remove(&hwnd);
            let _ = self.clear_ignore_identity(hwnd);
            info!("Re-admitted temporarily ignored window {}", hwnd);
            return IpcResponse::Ok;
        }
        IpcResponse::error(format!(
            "Window remains ignored: {}",
            readmit_failure_reason(outcome)
        ))
    }

    fn remove_managed_membership(&mut self, hwnd: u64) -> bool {
        let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) else {
            return false;
        };
        let viewport_width = self.viewport_width_for(monitor_id);
        let mut was_tiled = false;
        if let Some(workspace) = self
            .workspaces
            .get_mut(&monitor_id)
            .and_then(|workspaces| workspaces.get_mut(ws_idx))
        {
            if workspace.is_floating(hwnd) {
                workspace.remove_floating(hwnd);
            } else if workspace.remove_window(hwnd).is_ok() {
                was_tiled = true;
                workspace.ensure_focused_visible_animated(viewport_width);
            }
        }
        was_tiled
    }

    fn forget_managed_metadata(&mut self, hwnd: u64) {
        self.snap_disabled_hwnds.remove(&hwnd);
        self.window_managed_at.remove(&hwnd);
        self.window_last_maximized_at.remove(&hwnd);
        self.application_fullscreen.remove(&hwnd);
        self.last_placed_layout_rects.remove(&hwnd);
        self.clear_physical_window_state(hwnd);
        self.sticky_windows.remove(&hwnd);
        self.scratchpad_on_window_destroyed(hwnd);
        if self.previous_focused_hwnd == Some(hwnd) {
            self.hide_border();
            self.previous_focused_hwnd = None;
            let monitor = self.focused_monitor as i64;
            self.broadcast_focused_window_if_changed(monitor, None);
        }
    }

    fn restore_unmanaged_native_state(&mut self, hwnd: u64) -> Result<(), String> {
        let mut failures: Vec<String> = Vec::new();
        self.restore_snap_for_window(hwnd);
        self.release_departing_hwnd_ghost(hwnd);
        leopardwm_platform_win32::taskbar::taskbar_show(hwnd);
        #[cfg(not(test))]
        if let Err(error) = leopardwm_platform_win32::restore_window_moved_offscreen(hwnd) {
            failures.push(error.to_string());
        }
        #[cfg(test)]
        if let Some(error) = self.injected_native_restore_error.take() {
            failures.push(error);
        }
        self.forget_managed_metadata(hwnd);
        self.update_tab_strip();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    fn stamp_ignore_identity(&mut self, hwnd: u64) -> Result<u64, String> {
        #[cfg(test)]
        {
            if let Some(error) = self.injected_identity_stamp_error.take() {
                return Err(error);
            }
            let token = self.next_injected_lifetime_token;
            self.next_injected_lifetime_token = self.next_injected_lifetime_token.saturating_add(1);
            if self.next_injected_lifetime_token == 0 {
                self.next_injected_lifetime_token = 1;
            }
            self.injected_lifetime_tokens.insert(hwnd, token);
            Ok(token)
        }
        #[cfg(not(test))]
        leopardwm_platform_win32::stamp_window_lifetime_token(hwnd)
            .map_err(|error| error.to_string())
    }

    fn read_ignore_identity(&self, hwnd: u64) -> Result<Option<u64>, String> {
        #[cfg(test)]
        {
            if let Some(error) = &self.injected_identity_read_error {
                return Err(error.clone());
            }
            Ok(self.injected_lifetime_tokens.get(&hwnd).copied())
        }
        #[cfg(not(test))]
        leopardwm_platform_win32::read_window_lifetime_token(hwnd)
            .map_err(|error| error.to_string())
    }

    fn clear_ignore_identity(&mut self, hwnd: u64) -> Result<(), String> {
        #[cfg(test)]
        {
            if let Some(error) = self.injected_identity_clear_error.take() {
                return Err(error);
            }
            self.injected_lifetime_tokens.remove(&hwnd);
            Ok(())
        }
        #[cfg(not(test))]
        leopardwm_platform_win32::clear_window_lifetime_token(hwnd)
            .map_err(|error| error.to_string())
    }
}

fn readmit_failure_reason(outcome: AdmitOutcome) -> &'static str {
    match outcome {
        AdmitOutcome::PersistentIgnore => "persistent Ignore rule",
        AdmitOutcome::ElevationBlocked => "elevation blocked",
        AdmitOutcome::DialogLike => "window is ineligible",
        AdmitOutcome::NoWindowInfo => "window info is unavailable",
        AdmitOutcome::InsertFailed => "admission failed",
        AdmitOutcome::ShellCloaked => "window is shell-cloaked",
        AdmitOutcome::TransientConsoleHost => "transient console host",
        AdmitOutcome::TransientSuppressed => "window is transiently suppressed",
        AdmitOutcome::GatedIgnored => "window is temporarily ignored",
        AdmitOutcome::Admitted | AdmitOutcome::AlreadyManaged => "unexpected admission success",
    }
}
