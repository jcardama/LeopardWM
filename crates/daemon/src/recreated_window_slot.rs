use crate::state::AppState;
use leopardwm_core_layout::{LayoutError, Workspace};
use leopardwm_platform_win32::WindowInfo;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::info;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct WindowIdentity {
    process_id: u32,
    class_name: String,
}

#[derive(Default)]
pub(crate) struct RecreatedWindowSlots {
    pub(crate) identities: HashMap<u64, WindowIdentity>,
    pub(crate) slots: HashMap<WindowIdentity, RecreatedWindowSlot>,
    background_rejoins: HashMap<u64, BackgroundRejoinActivation>,
    pub(crate) suspended_at: Option<Instant>,
    pub(crate) resumed_at: Option<Instant>,
}

struct BackgroundRejoinActivation {
    managed_token: u64,
    admitted_at: Instant,
    admitted_at_event_ms: u32,
    selected_monitor: isize,
    selected_workspace: usize,
}

pub(crate) struct RecreatedWindowSlot {
    pub(crate) monitor: isize,
    pub(crate) workspace: usize,
    row: usize,
    column: usize,
    window_slot: usize,
    width: i32,
    sibling: Option<u64>,
    donor: u64,
    pub(crate) hidden_at: Instant,
}

impl RecreatedWindowSlot {
    pub(crate) fn insert(
        &self,
        workspace: &mut Workspace,
        hwnd: u64,
        take_focus: bool,
    ) -> Result<(), LayoutError> {
        if let Some((row, column, _)) = self.sibling.and_then(|s| workspace.find_window_location_rc(s)) {
            let focused = workspace.focused_window();
            workspace.insert_window_in_row_column_at(hwnd, row, column, self.window_slot)?;
            if take_focus {
                workspace.focus_window(hwnd)?;
            } else if let Some(focused) = focused {
                workspace.focus_window(focused)?;
            }
            Ok(())
        } else if take_focus {
            workspace.insert_window_at_row_column(hwnd, self.row, Some(self.width), self.column)
        } else {
            workspace.insert_window_at_row_column_no_focus(hwnd, self.row, Some(self.width), self.column)
        }
    }
}

impl AppState {
    pub(crate) fn handle_suspend_resume(
        &mut self,
        event: leopardwm_platform_win32::SuspendResumeEvent,
    ) {
        match event {
            leopardwm_platform_win32::SuspendResumeEvent::Suspend => self.handle_system_suspend(),
            leopardwm_platform_win32::SuspendResumeEvent::Resume => self.handle_system_resume(),
        }
    }

    pub(crate) fn handle_system_suspend(&mut self) {
        self.recreated_window_slots.suspended_at = Some(Instant::now());
        info!("System suspended");
    }

    pub(crate) fn handle_system_resume(&mut self) {
        self.recreated_window_slots.resumed_at = Some(Instant::now());
        info!("System resumed");
    }

    pub(crate) fn record_managed_window_identity(&mut self, window: &WindowInfo) {
        self.recreated_window_slots.identities.insert(
            window.hwnd,
            WindowIdentity {
                process_id: window.process_id,
                class_name: window.class_name.clone(),
            },
        );
    }

    pub(crate) fn arm_background_rejoin_activation(
        &mut self,
        hwnd: u64,
        event_time_ms: Option<u32>,
    ) {
        let interval =
            Duration::from_millis(u64::from(crate::event_handler::MINIMIZE_HANDOFF_WINDOW_MS));
        self.recreated_window_slots
            .background_rejoins
            .retain(|hwnd, guard| {
                guard.admitted_at.elapsed() < interval
                    && self.managed_lifetime_tokens.get(hwnd) == Some(&guard.managed_token)
            });
        let Some(&managed_token) = self.managed_lifetime_tokens.get(&hwnd) else {
            return;
        };
        let guard = BackgroundRejoinActivation {
            managed_token,
            admitted_at: Instant::now(),
            admitted_at_event_ms: event_time_ms.unwrap_or_else(|| self.event_time_now_ms()),
            selected_monitor: self.focused_monitor,
            selected_workspace: self.active_workspace_idx(self.focused_monitor),
        };
        self.recreated_window_slots
            .background_rejoins
            .insert(hwnd, guard);
    }

    pub(crate) fn suppress_background_rejoin_activation(
        &mut self,
        hwnd: u64,
        event_time_ms: u32,
    ) -> bool {
        let Some(guard) = self.recreated_window_slots.background_rejoins.get(&hwnd) else {
            return false;
        };
        let interval_ms = crate::event_handler::MINIMIZE_HANDOFF_WINDOW_MS;
        let eligible = guard.admitted_at.elapsed() < Duration::from_millis(u64::from(interval_ms))
            && self.managed_lifetime_tokens.get(&hwnd) == Some(&guard.managed_token)
            && !self.managed_lifetime_replaced(hwnd)
            && self.focused_monitor == guard.selected_monitor
            && self.active_workspace_idx(guard.selected_monitor) == guard.selected_workspace
            && crate::event_handler::event_time_is_no_later_than(
                event_time_ms,
                guard.admitted_at_event_ms.wrapping_add(interval_ms),
            )
            && self
                .find_window_workspace(hwnd)
                .is_some_and(|(monitor, workspace)| {
                    workspace != self.active_workspace_idx(monitor)
                });
        if !eligible {
            self.recreated_window_slots.background_rejoins.remove(&hwnd);
            return false;
        }
        self.release_parked_foreground();
        self.sync_foreground_window();
        true
    }

    pub(crate) fn forget_recreated_window_lifetime(&mut self, hwnd: u64) {
        self.recreated_window_slots.identities.remove(&hwnd);
        self.recreated_window_slots.background_rejoins.remove(&hwnd);
        for slot in self.recreated_window_slots.slots.values_mut() {
            if slot.sibling == Some(hwnd) {
                slot.sibling = None;
            }
        }
    }

    pub(crate) fn prepare_recreated_window_slot(
        &self,
        hwnd: u64,
    ) -> Option<(WindowIdentity, RecreatedWindowSlot)> {
        let identity = self.recreated_window_slots.identities.get(&hwnd)?.clone();
        let (monitor, workspace) = self.find_window_workspace(hwnd)?;
        let ws = self.workspaces.get(&monitor)?.get(workspace)?;
        let (row, column, window_slot) = ws.find_window_location_rc(hwnd)?;
        let col = ws.rows().get(row).and_then(|r| r.column(column))?;
        Some((
            identity,
            RecreatedWindowSlot {
                monitor,
                workspace,
                row,
                column,
                window_slot,
                width: col.width(),
                sibling: col.windows().iter().copied().find(|&s| s != hwnd),
                donor: hwnd,
                hidden_at: Instant::now(),
            },
        ))
    }

    pub(crate) fn donate_recreated_window_slot(
        &mut self,
        donation: Option<(WindowIdentity, RecreatedWindowSlot)>,
    ) {
        let Some((identity, slot)) = donation else {
            return;
        };
        let same_process_remains = self.managed_process_has_window(identity.process_id);
        self.recreated_window_slots
            .slots
            .retain(|_, slot| slot.hidden_at.elapsed() < Duration::from_secs(60));
        if !same_process_remains {
            self.recreated_window_slots.slots.insert(identity, slot);
        }
    }

    fn managed_process_has_window(&self, process_id: u32) -> bool {
        self.recreated_window_slots
            .identities
            .iter()
            .any(|(&hwnd, identity)| {
                identity.process_id == process_id && self.is_managed_member(hwnd)
            })
    }

    pub(crate) fn take_recreated_window_slot(
        &mut self,
        window: &WindowInfo,
        kind: crate::event_handler::AdmissionKind,
        action: crate::config::WindowAction,
        sticky: bool,
    ) -> Option<RecreatedWindowSlot> {
        let same_process_remains = self.managed_process_has_window(window.process_id);
        let state = &mut self.recreated_window_slots;
        state
            .slots
            .retain(|_, slot| slot.hidden_at.elapsed() < Duration::from_secs(60));
        if kind != crate::event_handler::AdmissionKind::Automatic
            || action != crate::config::WindowAction::Tile
            || sticky
        {
            return None;
        }
        let key = WindowIdentity {
            process_id: window.process_id,
            class_name: window.class_name.clone(),
        };
        if same_process_remains {
            state.slots.remove(&key);
            return None;
        }
        let slot = state.slots.get(&key)?;
        let suspend = state.suspended_at?;
        let resume = state.resumed_at?;
        if slot.donor == window.hwnd
            || suspend >= slot.hidden_at
            || resume <= suspend
            || slot.hidden_at.saturating_duration_since(resume) > Duration::from_secs(120)
            || !self.monitors.contains_key(&slot.monitor)
            || self
                .workspaces
                .get(&slot.monitor)
                .and_then(|workspaces| workspaces.get(slot.workspace))
                .is_none()
        {
            return None;
        }
        state.slots.remove(&key)
    }
}
