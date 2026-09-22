//! Session-only identity for managed HWNDs.
//!
//! Separate from temporary-ignore tokens. A delayed Destroyed event keeps
//! membership only when the managed property still matches the token recorded
//! for that admission.

use crate::state::{AppState, DRAG_PLACEHOLDER_HWND};
use crate::temporary_ignore::IdentityReadError;
#[cfg(not(test))]
use leopardwm_platform_win32::Win32Error;
use tracing::debug;

impl AppState {
    /// Stamp and record a managed lifetime. Stamp failure logs and leaves no record.
    pub(crate) fn record_managed_lifetime(&mut self, hwnd: u64) {
        if hwnd == DRAG_PLACEHOLDER_HWND {
            return;
        }
        match self.stamp_managed_identity(hwnd) {
            Ok(token) => {
                self.managed_lifetime_tokens.insert(hwnd, token);
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
        self.record_managed_lifetime(hwnd);
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

    /// Whether Destroyed should keep the current lifetime.
    ///
    /// A managed member with a record is kept only when the managed token
    /// matches or the read is transient. No record uses the pre-existing
    /// ignore-lifetime liveness check, so unrecorded members and temporary
    /// ignores behave as before.
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

    /// `true` when admission must stop because this HWND is still the recorded window.
    ///
    /// A replaced member, tiled drag source, or designated scratchpad gets the
    /// full Destroyed departure first, including layout and focus, so a later
    /// admission failure does not leave the old lifetime half-removed. Admission
    /// then continues.
    pub(crate) fn duplicate_managed_admission(&mut self, hwnd: u64) -> bool {
        if !self.is_managed_member(hwnd) {
            return false;
        }
        if !self.managed_lifetime_replaced(hwnd) {
            debug!("Window {hwnd} already managed, ignoring create event");
            return true;
        }
        debug!("Departing recycled managed hwnd {hwnd} so the replacement can be admitted");
        self.depart_destroyed_or_hidden_window(hwnd, false);
        // This Created already passed transient suppression. A short-lived old
        // lifetime must not leave a suppression entry for the replacement.
        self.recently_hidden_hwnds.remove(&hwnd);
        false
    }

    fn is_managed_member(&self, hwnd: u64) -> bool {
        self.find_window_workspace(hwnd).is_some()
            || self
                .drag_state
                .as_ref()
                .is_some_and(|drag| drag.hwnd == hwnd && drag.is_tiled)
            || self.scratchpad.map(|pad| pad.window_id) == Some(hwnd)
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
