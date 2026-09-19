//! Session-only temporary ignore for the actual OS foreground window.

use crate::event_handler::{AdmissionKind, AdmitOutcome};
use crate::layout_apply::LayoutApplyOutcome;
use crate::state::AppState;
use leopardwm_ipc::IpcResponse;
#[cfg(not(test))]
use leopardwm_platform_win32::Win32Error;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IdentityReadError {
    Gone,
    Transient(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdleLayoutReapply {
    NotPending,
    Applied,
    Waiting,
    DeferredFailure,
    Paused,
}

const MAX_IDLE_LAYOUT_REAPPLY_FAILURES: u8 = 3;

impl std::fmt::Display for IdentityReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gone => write!(f, "window not found"),
            Self::Transient(message) => write!(f, "{message}"),
        }
    }
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
            Ok(Some(_)) | Ok(None) | Err(IdentityReadError::Gone) => {
                self.retire_stale_temporary_ignore(hwnd, entry.token);
                IgnoreGate::Allow
            }
            Err(IdentityReadError::Transient(error)) => {
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
            Ok(Some(_)) | Ok(None) | Err(IdentityReadError::Gone) => {
                self.retire_stale_temporary_ignore(hwnd, entry.token);
            }
            Err(IdentityReadError::Transient(error)) => {
                debug!(
                    "Keeping temporary ignore for {} after destroy identity failure: {}",
                    hwnd, error
                );
            }
        }
    }

    /// Production `Ok(_)` means `IsWindow` succeeded. Tests cannot call
    /// `IsWindow` on synthetic HWNDs, so `Ok(None)` there is only a missing
    /// token unless a test injects live-unmarked proof.
    pub(crate) fn hwnd_lifetime_is_currently_live(&self, hwnd: u64) -> bool {
        match self.read_ignore_identity(hwnd) {
            Ok(Some(_)) => true,
            Ok(None) => {
                #[cfg(test)]
                {
                    self.injected_live_hwnds.contains(&hwnd)
                }
                #[cfg(not(test))]
                {
                    true
                }
            }
            Err(IdentityReadError::Gone) => false,
            Err(IdentityReadError::Transient(_)) => true,
        }
    }

    pub(crate) fn temporary_ignore_is_live(&self, hwnd: u64) -> bool {
        let Some(entry) = self.temporary_ignores.get(&hwnd) else {
            return false;
        };
        match self.read_ignore_identity(hwnd) {
            Ok(Some(token)) if token == entry.token => true,
            Ok(Some(_)) | Ok(None) | Err(IdentityReadError::Gone) => false,
            Err(IdentityReadError::Transient(_)) => true,
        }
    }

    pub(crate) fn temporary_ignore_is_dead(&self, hwnd: u64) -> bool {
        matches!(
            self.read_ignore_identity(hwnd),
            Err(IdentityReadError::Gone)
        )
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
            self.request_idle_layout_reapply();
            return IpcResponse::error(format!("Window remains managed: {reason}"));
        }
        match self.read_ignore_identity(hwnd) {
            Ok(Some(live)) if live == token => {}
            Ok(_) | Err(IdentityReadError::Gone) => {
                return self.abandon_stale_unmanage(hwnd);
            }
            Err(IdentityReadError::Transient(error)) => {
                self.request_idle_layout_reapply();
                return IpcResponse::error(format!("Window remains managed: {error}"));
            }
        }

        self.cancel_matching_unfinished_move_size_ui(hwnd);
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
        let layout_result = self.reflow_peers_after_unmanage(hwnd, was_tiled, snapshot);
        match (restore_result, layout_result) {
            (Ok(()), Ok(())) => {
                info!(
                    "Temporarily ignored window {} (pid {} class {})",
                    hwnd, process_id, class_name
                );
                IpcResponse::Ok
            }
            (Err(error), Ok(())) => IpcResponse::error(format!(
                "Window was unmanaged but native restore failed: {error}"
            )),
            (Ok(()), Err(error)) => {
                IpcResponse::error(format!("Window was unmanaged but layout failed: {error}"))
            }
            (Err(restore), Err(layout)) => IpcResponse::error(format!(
                "Window was unmanaged but native restore failed: {restore}; layout failed: {layout}"
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
            Ok(Some(_)) | Ok(None) | Err(IdentityReadError::Gone) => {
                self.retire_stale_temporary_ignore(hwnd, entry.token);
                return IpcResponse::error(
                    "Foreground window is not the ignored lifetime and was not re-admitted",
                );
            }
            Err(IdentityReadError::Transient(error)) => {
                return IpcResponse::error(format!("Window remains ignored: {error}"));
            }
        }
        if let Err(reason) = self.drain_pending_placement_work() {
            self.request_idle_layout_reapply();
            return IpcResponse::error(format!("Window remains ignored: {reason}"));
        }
        let outcome = self.try_admit_window(hwnd, AdmissionKind::ExplicitReadmit);
        match outcome {
            AdmitOutcome::Admitted | AdmitOutcome::AlreadyManaged => {
                self.commit_readmit(hwnd);
                IpcResponse::Ok
            }
            AdmitOutcome::AdmittedPlacementFailed => {
                self.commit_readmit(hwnd);
                IpcResponse::error(
                    "Window was re-admitted but layout failed; it remains managed and is no longer ignored",
                )
            }
            other => {
                self.request_idle_layout_reapply();
                IpcResponse::error(format!(
                    "Window remains ignored: {}",
                    readmit_failure_reason(other)
                ))
            }
        }
    }

    fn commit_readmit(&mut self, hwnd: u64) {
        self.temporary_ignores.remove(&hwnd);
        let _ = self.clear_ignore_identity(hwnd);
        self.adopt_os_foreground_without_stealing_focus(hwnd);
        info!("Re-admitted temporarily ignored window {}", hwnd);
    }

    fn adopt_os_foreground_without_stealing_focus(&mut self, hwnd: u64) {
        let Some((monitor_id, _)) = self.find_window_workspace(hwnd) else {
            return;
        };
        self.focused_monitor = monitor_id;
        self.previous_focused_hwnd = Some(hwnd);
        self.last_focus_change_at = Some(std::time::Instant::now());
        self.show_border(hwnd);
        self.broadcast_focused_window_if_changed(monitor_id as i64, Some(hwnd));
    }

    fn abandon_stale_unmanage(&mut self, hwnd: u64) -> IpcResponse {
        self.cancel_matching_unfinished_move_size_ui(hwnd);
        let snapshot = self.snapshot_layout();
        let was_tiled = self.remove_managed_membership(hwnd);
        self.forget_managed_metadata(hwnd);
        let layout_result = self.reflow_peers_after_unmanage(hwnd, was_tiled, snapshot);
        let mut message =
            "Foreground window is no longer the stamped lifetime and was not ignored".to_string();
        if let Err(error) = layout_result {
            message = format!("{message}; layout failed: {error}");
        }
        IpcResponse::error(message)
    }

    fn reflow_peers_after_unmanage(
        &mut self,
        hwnd: u64,
        was_tiled: bool,
        mut snapshot: std::collections::HashMap<u64, leopardwm_core_layout::Rect>,
    ) -> Result<(), String> {
        if was_tiled {
            snapshot.remove(&hwnd);
            if self.start_layout_transition(snapshot) {
                if let Some(ref mut transition) = self.layout_transition {
                    transition.suppress_landing_focus_resync = true;
                }
            }
        }
        match self.apply_layout() {
            Ok(LayoutApplyOutcome::Completed) => Ok(()),
            Ok(LayoutApplyOutcome::DeferredByRecoveryBarrier) => {
                Err("Layout reflow remains pending while animation placement finishes".into())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    fn request_idle_layout_reapply(&mut self) {
        self.pending_idle_layout_reapply = true;
        self.idle_layout_reapply_failures = 0;
        let _ = self.try_consume_idle_layout_reapply();
    }

    pub(crate) fn try_consume_idle_layout_reapply(&mut self) -> IdleLayoutReapply {
        if !self.pending_idle_layout_reapply {
            return IdleLayoutReapply::NotPending;
        }
        self.reap_finished_pending_apply_workers();
        if !self.pending_apply_workers.is_empty() {
            return IdleLayoutReapply::Waiting;
        }
        if !self.animation_placement_worker_is_idle() {
            return IdleLayoutReapply::Waiting;
        }
        if self.paused {
            return IdleLayoutReapply::Paused;
        }
        if !self
            .apply_worker_cancelled
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            if let Some(remaining) = self
                .layout_transition
                .as_ref()
                .map(|transition| transition.duration_ms.saturating_sub(transition.elapsed_ms))
            {
                self.tick_animations(remaining);
            }
        }
        match self.apply_layout() {
            Ok(LayoutApplyOutcome::Completed) => {}
            Ok(LayoutApplyOutcome::DeferredByRecoveryBarrier) => return IdleLayoutReapply::Waiting,
            Err(error) => {
                self.idle_layout_reapply_failures =
                    self.idle_layout_reapply_failures.saturating_add(1);
                if self.idle_layout_reapply_failures >= MAX_IDLE_LAYOUT_REAPPLY_FAILURES {
                    warn!(
                        "Deferring temporary-ignore layout recovery after {} failed attempts: {}",
                        self.idle_layout_reapply_failures, error
                    );
                    return IdleLayoutReapply::DeferredFailure;
                }
                warn!(
                    "Temporary-ignore layout recovery attempt {} failed; retrying: {}",
                    self.idle_layout_reapply_failures, error
                );
                return IdleLayoutReapply::Waiting;
            }
        }
        if !self.pending_idle_layout_reapply {
            IdleLayoutReapply::Applied
        } else if self.paused {
            IdleLayoutReapply::Paused
        } else {
            IdleLayoutReapply::Waiting
        }
    }

    pub(crate) fn idle_layout_reapply_timer_needed(&self) -> bool {
        self.pending_idle_layout_reapply
            && !self.paused
            && self.idle_layout_reapply_failures < MAX_IDLE_LAYOUT_REAPPLY_FAILURES
    }

    pub(crate) fn animation_placement_worker_is_idle(&self) -> bool {
        self.animation_worker_control
            .as_ref()
            .is_none_or(|control| control.wait_for_barrier(std::time::Duration::from_millis(1)))
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
        leopardwm_platform_win32::snapshot::snapshot_remove(hwnd);
        self.tab_title_overrides.remove(&hwnd);
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
        #[cfg(not(test))]
        leopardwm_platform_win32::dwm_uncloak_window(hwnd);
        #[cfg(test)]
        self.injected_native_uncloak_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.forget_managed_metadata(hwnd);
        self.update_tab_strip();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    fn retire_stale_temporary_ignore(&mut self, hwnd: u64, token: u64) {
        self.forget_recycled_temporary_ignore_caches(hwnd);
        self.remove_temporary_ignore_if_token(hwnd, token);
    }

    fn forget_recycled_temporary_ignore_caches(&mut self, hwnd: u64) {
        self.overview_icon_cache.remove(&hwnd);
        self.hidden_column_widths.remove(&hwnd);
        self.move_origins.remove(&hwnd);
        for origin in self.move_origins.values_mut() {
            if origin.sibling == Some(hwnd) {
                origin.sibling = None;
            }
        }
        self.floating_focus.retain(|_, focused| *focused != hwnd);
        leopardwm_platform_win32::taskbar::taskbar_forget(hwnd);
        leopardwm_platform_win32::clear_suspected_oversize(hwnd);
        for layout in self.stashed_monitor_layouts.values_mut() {
            for workspace in &mut layout.workspaces {
                let _ = workspace.remove_window(hwnd);
                workspace.remove_floating(hwnd);
            }
        }
        self.stashed_monitor_layouts.retain(|_, layout| {
            layout.workspaces.iter().any(|workspace| {
                workspace.window_count() > 0 || !workspace.floating_windows().is_empty()
            })
        });
    }

    fn remove_temporary_ignore_if_token(&mut self, hwnd: u64, token: u64) {
        if self
            .temporary_ignores
            .get(&hwnd)
            .is_some_and(|current| current.token == token)
        {
            self.temporary_ignores.remove(&hwnd);
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

    fn read_ignore_identity(&self, hwnd: u64) -> Result<Option<u64>, IdentityReadError> {
        #[cfg(test)]
        {
            if let Some(override_value) = &self.injected_identity_read_override {
                return override_value.clone();
            }
            if let Some(error) = &self.injected_identity_read_error {
                return Err(error.clone());
            }
            Ok(self.injected_lifetime_tokens.get(&hwnd).copied())
        }
        #[cfg(not(test))]
        match leopardwm_platform_win32::read_window_lifetime_token(hwnd) {
            Ok(token) => Ok(token),
            Err(Win32Error::WindowNotFound(_)) => Err(IdentityReadError::Gone),
            Err(error) => Err(IdentityReadError::Transient(error.to_string())),
        }
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
        AdmitOutcome::Admitted
        | AdmitOutcome::AlreadyManaged
        | AdmitOutcome::AdmittedPlacementFailed => "unexpected admission success",
    }
}
