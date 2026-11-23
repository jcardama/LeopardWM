//! Managed HWND identity is separate from temporary-ignore identity. A delayed
//! Destroyed event preserves a recorded member when its property matches or
//! the identity read is transient.

use crate::state::{AppState, ScratchpadState, DRAG_PLACEHOLDER_HWND};
use crate::temporary_ignore::IdentityReadError;
#[cfg(not(test))]
use leopardwm_platform_win32::Win32Error;
use tracing::debug;

/// A stashed scratchpad is managed ownership outside every workspace. A shown
/// scratchpad is an ordinary floating member and must not keep that ownership.
pub(crate) fn is_stashed_scratchpad(scratchpad: Option<ScratchpadState>, hwnd: u64) -> bool {
    scratchpad.is_some_and(|pad| !pad.shown && pad.window_id == hwnd)
}

impl AppState {
    /// Stamp and record a managed lifetime; log stamp failures.
    ///
    /// `admitted_at_event_ms` is the triggering Create/Show time. Eventless
    /// admission records no Hidden-guard time and drops any older one.
    pub(crate) fn record_managed_lifetime(&mut self, hwnd: u64, admitted_at_event_ms: Option<u32>) {
        if hwnd == DRAG_PLACEHOLDER_HWND {
            return;
        }
        self.post_admission_maximize_restore_eligible.remove(&hwnd);
        match self.stamp_managed_identity(hwnd) {
            Ok(token) => {
                self.managed_lifetime_tokens.insert(hwnd, token);
                match admitted_at_event_ms {
                    Some(event_time_ms) => {
                        self.managed_lifetime_admitted_at_event_ms
                            .insert(hwnd, event_time_ms);
                    }
                    None => {
                        self.managed_lifetime_admitted_at_event_ms.remove(&hwnd);
                    }
                }
            }
            Err(error) => {
                debug!("Failed to stamp managed lifetime for {hwnd}: {error}");
            }
        }
    }

    /// Record only when this session has no token yet. Enumeration must not
    /// restamp an already-recorded window: a new token would look like a recycle.
    pub(crate) fn record_managed_lifetime_if_unrecorded(&mut self, hwnd: u64) {
        if self.managed_lifetime_tokens.contains_key(&hwnd) {
            return;
        }
        self.record_managed_lifetime(hwnd, None);
    }

    /// The recorded managed lifetime is not the one on the HWND now.
    ///
    /// Missing or a different token is replacement proof. `Gone` is not: tests
    /// use synthetic HWNDs that are not live, and a dead read must not drop
    /// membership on Created. Transient reads are not proof either.
    pub(crate) fn managed_lifetime_replaced(&self, hwnd: u64) -> bool {
        let Some(&recorded) = self.managed_lifetime_tokens.get(&hwnd) else {
            return false;
        };
        match self.read_managed_identity(hwnd) {
            Ok(None) => true,
            Ok(Some(token)) => token != recorded,
            Err(_) => false,
        }
    }

    /// A recorded member survives Destroyed only on a matching token or a
    /// transient read. Without a record, use ignore-lifetime liveness instead.
    pub(crate) fn destroyed_names_current_lifetime(&self, hwnd: u64) -> bool {
        if self.is_managed_member(hwnd) {
            if let Some(&recorded) = self.managed_lifetime_tokens.get(&hwnd) {
                return match self.read_managed_identity(hwnd) {
                    Ok(Some(token)) if token == recorded => true,
                    Err(IdentityReadError::Transient(_)) => true,
                    Ok(Some(_)) | Ok(None) | Err(IdentityReadError::Gone) => false,
                };
            }
        }
        self.hwnd_lifetime_is_currently_live(hwnd)
    }

    /// Depart a managed member whose recorded lifetime is no longer on the HWND.
    ///
    /// Returns true when this call ran the full Destroyed departure. Admission
    /// and enumeration then evaluate the replacement as a new window.
    pub(crate) fn depart_replaced_managed_lifetime(&mut self, hwnd: u64) -> bool {
        if !self.is_managed_member(hwnd) || !self.managed_lifetime_replaced(hwnd) {
            return false;
        }
        debug!("Departing recycled managed hwnd {hwnd} so the replacement can be admitted");
        self.depart_destroyed_or_hidden_window(
            hwnd,
            false,
            crate::ui_sync::DepartureCause::ReplacedLifetime,
        );
        // Drop the entry this departure, or an earlier cloak Hidden, recorded
        // so it cannot reject the replacement. Created checks suppression after
        // this helper; enumeration does not.
        self.recently_hidden_hwnds.remove(&hwnd);
        true
    }

    /// `true` when admission must stop because this HWND is still the recorded window.
    ///
    /// A replaced member, tiled drag source, or stashed scratchpad gets the
    /// full Destroyed departure first, including layout and focus, so a later
    /// admission failure does not leave the old lifetime half-removed. Admission
    /// then continues.
    pub(crate) fn duplicate_managed_admission(&mut self, hwnd: u64) -> bool {
        if self.depart_replaced_managed_lifetime(hwnd) {
            return false;
        }
        if !self.is_managed_member(hwnd) {
            return false;
        }
        debug!("Window {hwnd} already managed, ignoring create event");
        true
    }

    /// Live managed stamp, if the property can be read.
    ///
    /// `None` is a missing property or a failed read, not a recorded lifetime.
    /// Callers that store a Hidden entry fall back to the departing recorded
    /// token. Legacy `None` is only when this session never recorded one.
    pub(crate) fn readable_managed_token(&self, hwnd: u64) -> Option<u64> {
        self.read_managed_identity(hwnd).ok().flatten()
    }

    pub(crate) fn managed_token_for_hidden_record(
        &self,
        hwnd: u64,
        recorded: Option<u64>,
    ) -> Option<u64> {
        self.readable_managed_token(hwnd).or(recorded)
    }

    pub(crate) fn take_managed_lifetime_token(&mut self, hwnd: u64) -> Option<u64> {
        leopardwm_platform_win32::clear_suspected_oversize(hwnd);
        self.window_move_recheck_attempts.remove(&hwnd);
        self.deferred_moved_or_resized.remove(&hwnd);
        self.deferred_physical_landings.remove(&hwnd);
        self.size_ignored_windows.remove(&hwnd);
        self.pending_maximized_admission_restores.remove(&hwnd);
        self.post_admission_maximize_restore_eligible.remove(&hwnd);
        self.forget_recreated_window_lifetime(hwnd);
        self.managed_lifetime_admitted_at_event_ms.remove(&hwnd);
        leopardwm_platform_win32::forget_stale_frame_refresh(hwnd);
        self.managed_lifetime_tokens.remove(&hwnd)
    }

    /// Create/Show time of the lifetime that currently owns `hwnd`, if one was recorded.
    ///
    /// No recorded time means the stale-Hidden guard does not apply.
    pub(crate) fn admitted_event_time_ms_if_current_member(&self, hwnd: u64) -> Option<u32> {
        if !self.is_managed_member(hwnd) {
            return None;
        }
        self.managed_lifetime_admitted_at_event_ms
            .get(&hwnd)
            .copied()
    }

    /// Whether a suppression entry still names `hwnd`'s current lifetime.
    ///
    /// A stored `None` cannot distinguish and suppresses. A stored token
    /// suppresses only when the current managed token equals it. A live window
    /// whose property is missing or different is a new lifetime. A transient
    /// or gone read is not a new token, so it keeps suppression.
    pub(crate) fn recently_hidden_names_current_lifetime(
        &self,
        stored: Option<u64>,
        hwnd: u64,
    ) -> bool {
        let Some(stored) = stored else {
            return true;
        };
        match self.read_managed_identity(hwnd) {
            Ok(Some(current)) => current == stored,
            Ok(None) => false,
            Err(_) => true,
        }
    }

    pub(crate) fn is_managed_member(&self, hwnd: u64) -> bool {
        self.find_window_workspace(hwnd).is_some()
            || self
                .drag_state
                .as_ref()
                .is_some_and(|drag| drag.hwnd == hwnd && drag.is_tiled)
            || is_stashed_scratchpad(self.scratchpad, hwnd)
    }

    fn stamp_managed_identity(&mut self, hwnd: u64) -> Result<u64, String> {
        #[cfg(test)]
        {
            let token = self.next_injected_lifetime_token;
            self.next_injected_lifetime_token = self.next_injected_lifetime_token.saturating_add(1);
            if self.next_injected_lifetime_token == 0 {
                self.next_injected_lifetime_token = 1;
            }
            self.injected_managed_tokens.insert(hwnd, token);
            Ok(token)
        }
        #[cfg(not(test))]
        leopardwm_platform_win32::stamp_managed_lifetime_token(hwnd)
            .map_err(|error| error.to_string())
    }

    fn read_managed_identity(&self, hwnd: u64) -> Result<Option<u64>, IdentityReadError> {
        #[cfg(test)]
        {
            if let Some(error) = &self.injected_identity_read_error {
                return Err(error.clone());
            }
            // Same liveness proof as `hwnd_lifetime_is_currently_live`. The
            // ignore read override does not apply to managed tokens.
            let live = self.injected_live_hwnds.contains(&hwnd)
                || self.injected_lifetime_tokens.contains_key(&hwnd);
            if !live {
                return Err(IdentityReadError::Gone);
            }
            Ok(self.injected_managed_tokens.get(&hwnd).copied())
        }
        #[cfg(not(test))]
        match leopardwm_platform_win32::read_managed_lifetime_token(hwnd) {
            Ok(token) => Ok(token),
            Err(Win32Error::WindowNotFound(_)) => Err(IdentityReadError::Gone),
            Err(error) => Err(IdentityReadError::Transient(error.to_string())),
        }
    }
}
