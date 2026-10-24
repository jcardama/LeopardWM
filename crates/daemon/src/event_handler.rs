use crate::config;
use crate::state::{
    AppState, ApplicationFullscreenState, DragHintAction, DragState, ElevationBlockedRecord,
    HiddenColumnWidth, LastWindowDepartureOrigin, PendingLastWindowDeparture, RecentlyHiddenEntry,
    WindowMoveRecheckAttempt, EDIT_CONFIG_PULL_TTL, FALLBACK_VIEWPORT_HEIGHT,
    FALLBACK_VIEWPORT_WIDTH, RECENTLY_HIDDEN_TTL, RECENTLY_RESTORED_MANAGED_WINDOW_TTL,
    TRANSIENT_WINDOW_THRESHOLD, WINDOW_MOVE_RECHECK_FIGHT_WINDOW,
};
use crate::ui_sync::DepartureCause;
use leopardwm_core_layout::{Rect, Visibility, Workspace};
#[cfg(not(test))]
use leopardwm_platform_win32::enumerate_monitors;
use leopardwm_platform_win32::{
    find_monitor_for_rect, get_process_executable, is_shift_key_pressed, MonitorInfo, WindowEvent,
};
use tracing::{debug, info, warn};

pub(crate) const MINIMIZE_HANDOFF_WINDOW_MS: u32 = 500; // Windows timestamps post-minimize focus handoff events after the minimize.

/// How long after a window is first managed to treat it as still settling its
/// initial geometry.
pub(crate) const SNAPBACK_SETTLE_AFTER_CREATE: std::time::Duration =
    std::time::Duration::from_millis(2000);
const POST_ADMISSION_MAXIMIZE_RESTORE_BUDGET: u8 = 4;
/// How recently a window must have been seen maximized to defer snapping it back
/// while settling.
pub(crate) const SNAPBACK_MAXIMIZE_GRACE: std::time::Duration =
    std::time::Duration::from_millis(1200);

/// Whether to defer snapping a tiled window back to its layout slot because it
/// opened maximized and is still settling. An app opening several windows/tabs
/// at once can momentarily report a restored size between maximize passes;
/// snapping then tiles the window narrow (the reported "shrinks to minimum").
/// Deferred only while the window is both freshly managed and was maximized very
/// recently, so a normal manual un-maximize of an established window still
/// re-tiles immediately.
pub(crate) fn defer_snapback_while_settling(
    managed_at: Option<std::time::Instant>,
    last_maximized_at: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    let settling =
        managed_at.is_some_and(|t| now.saturating_duration_since(t) < SNAPBACK_SETTLE_AFTER_CREATE);
    let recently_maximized = last_maximized_at
        .is_some_and(|t| now.saturating_duration_since(t) < SNAPBACK_MAXIMIZE_GRACE);
    settling && recently_maximized
}

fn is_transient_console_host(title: &str, class_name: &str, executable: &str) -> bool {
    let title_lower = title.to_ascii_lowercase();
    class_name == "ConsoleWindowClass"
        && (title_lower.ends_with(".exe")
            || (!executable.is_empty() && title_lower == executable.to_ascii_lowercase()))
}

#[allow(clippy::too_many_arguments)]
fn insert_admitted_tile(
    workspace: &mut Workspace,
    hwnd: u64,
    width: Option<i32>,
    rule_slot: Option<usize>,
    rule_row: Option<usize>,
    in_column: bool,
    take_focus: bool,
    recreated_slot: Option<&crate::recreated_window_slot::RecreatedWindowSlot>,
) -> bool {
    if let Some(slot) = recreated_slot {
        slot.insert(workspace, hwnd, take_focus).is_ok()
    } else if let Some(target_row) = rule_row {
        if let Some(slot) = rule_slot {
            if take_focus {
                workspace
                    .insert_window_at_row_column(hwnd, target_row, width, slot)
                    .is_ok()
            } else {
                workspace
                    .insert_window_at_row_column_no_focus(hwnd, target_row, width, slot)
                    .is_ok()
            }
        } else if take_focus {
            workspace
                .insert_window_in_row(hwnd, target_row, width)
                .is_ok()
        } else {
            workspace
                .insert_window_in_row_no_focus(hwnd, target_row, width)
                .is_ok()
        }
    } else if let Some(slot) = rule_slot {
        if take_focus {
            workspace.insert_window_at_column(hwnd, width, slot).is_ok()
        } else {
            workspace
                .insert_window_at_column_no_focus(hwnd, width, slot)
                .is_ok()
        }
    } else if in_column {
        let col = workspace.focused_column_index();
        let row = workspace.focused_window_index_in_column() + 1;
        let ok = workspace.insert_window_in_column_at(hwnd, col, row).is_ok();
        if ok && take_focus {
            if let Err(e) = workspace.focus_window(hwnd) {
                warn!("Focusing new in-column window {} failed: {:?}", hwnd, e);
            }
        }
        ok
    } else if take_focus {
        workspace.insert_window(hwnd, width).is_ok()
    } else {
        workspace.insert_window_no_focus(hwnd, width).is_ok()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MoveSizeCancelResult {
    pub cancelled_resize: bool,
    pub cancelled_drag: bool,
    pub removed_placeholder: bool,
    pub removed_from_source: bool,
    pub drag_source: Option<(leopardwm_platform_win32::MonitorId, usize)>,
}

impl MoveSizeCancelResult {
    pub(crate) fn needs_layout(self) -> bool {
        self.removed_placeholder || self.removed_from_source
    }
}

/// Non-user focus must not break monocle; explicit user focus may leave it.
pub(crate) fn fullscreen_focus_guard(
    user_initiated: bool,
    fullscreen: Option<u64>,
    focused_hwnd: u64,
) -> Option<u64> {
    if user_initiated {
        return None;
    }
    fullscreen.filter(|&fs| fs != focused_hwnd)
}

/// Compare nearby timestamps in GetTickCount's wrapping u32 domain.
pub(crate) fn event_time_is_no_later_than(event_time_ms: u32, armed_time_ms: u32) -> bool {
    armed_time_ms.wrapping_sub(event_time_ms) < 0x8000_0000
}

pub(crate) fn workspace_is_genuinely_empty(workspace: &Workspace) -> bool {
    workspace.window_count() == 0 && workspace.floating_windows().is_empty()
}

pub(crate) fn detect_application_fullscreen<'a>(
    monitors: impl IntoIterator<Item = &'a MonitorInfo>,
    chrome_rect: Option<Rect>,
    dwm_rect: Option<Rect>,
    is_zoomed: bool,
) -> Option<ApplicationFullscreenState> {
    if is_zoomed {
        return None;
    }
    let rect = chrome_rect.or(dwm_rect)?;
    let monitor = monitors
        .into_iter()
        .find(|monitor| rect_matches_monitor(rect, monitor))?;
    Some(ApplicationFullscreenState {
        monitor_id: monitor.id,
        rect: monitor.rect,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplicationFullscreenLifecycle {
    Enter,
    Continue,
    Reassign,
    Exit,
    None,
}

pub(crate) fn application_fullscreen_lifecycle(
    tracked: Option<ApplicationFullscreenState>,
    observed: Option<ApplicationFullscreenState>,
) -> ApplicationFullscreenLifecycle {
    match (tracked, observed) {
        (None, Some(_)) => ApplicationFullscreenLifecycle::Enter,
        (Some(previous), Some(current)) if previous.monitor_id != current.monitor_id => {
            ApplicationFullscreenLifecycle::Reassign
        }
        (Some(_), Some(_)) => ApplicationFullscreenLifecycle::Continue,
        (Some(_), None) => ApplicationFullscreenLifecycle::Exit,
        (None, None) => ApplicationFullscreenLifecycle::None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MovedOrResizedDecision {
    Fullscreen(ApplicationFullscreenLifecycle),
    Suppress,
    Continue,
}

pub(crate) fn should_observe_maximize_during_suppression(
    applying_layout: bool,
    display_change_pending: bool,
    managed_tiled: bool,
    application_fullscreen: bool,
    is_maximized: bool,
) -> bool {
    applying_layout
        && !display_change_pending
        && managed_tiled
        && !application_fullscreen
        && is_maximized
}

pub(crate) fn moved_or_resized_decision(
    lifecycle: ApplicationFullscreenLifecycle,
    ordinary_suppressed: bool,
) -> MovedOrResizedDecision {
    if lifecycle != ApplicationFullscreenLifecycle::None {
        MovedOrResizedDecision::Fullscreen(lifecycle)
    } else if ordinary_suppressed {
        MovedOrResizedDecision::Suppress
    } else {
        MovedOrResizedDecision::Continue
    }
}

pub(crate) fn application_fullscreen_expected_layout_rect(
    current_layout_rect: Option<Rect>,
    last_placed_layout_rect: Option<Rect>,
) -> Option<Rect> {
    current_layout_rect.or(last_placed_layout_rect)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplicationFullscreenReconciliation {
    Retain,
    Update,
    Exit,
}

pub(crate) fn application_fullscreen_reconciliation(
    is_valid: bool,
    is_managed: bool,
    is_zoomed: bool,
    stored: ApplicationFullscreenState,
    observed: Option<ApplicationFullscreenState>,
    current_rect: Option<Rect>,
    tolerance: i32,
) -> ApplicationFullscreenReconciliation {
    if !is_valid || !is_managed || is_zoomed {
        return ApplicationFullscreenReconciliation::Exit;
    }
    if observed.is_some() {
        return ApplicationFullscreenReconciliation::Update;
    }
    if current_rect.is_some_and(|rect| rects_match_with_tolerance(rect, stored.rect, tolerance)) {
        ApplicationFullscreenReconciliation::Retain
    } else {
        ApplicationFullscreenReconciliation::Exit
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplicationFullscreenExitRoute {
    FloatingPreserve,
    MaximizedAllow,
    InactivePark,
    ActiveTiledApply,
}

pub(crate) fn application_fullscreen_exit_route(
    is_floating: bool,
    is_maximized: bool,
    is_active_workspace: bool,
) -> ApplicationFullscreenExitRoute {
    if is_floating {
        ApplicationFullscreenExitRoute::FloatingPreserve
    } else if is_maximized {
        ApplicationFullscreenExitRoute::MaximizedAllow
    } else if !is_active_workspace {
        ApplicationFullscreenExitRoute::InactivePark
    } else {
        ApplicationFullscreenExitRoute::ActiveTiledApply
    }
}

pub(crate) fn application_fullscreen_exit_restores_border(
    route: ApplicationFullscreenExitRoute,
    is_focused: bool,
) -> bool {
    is_focused
        && matches!(
            route,
            ApplicationFullscreenExitRoute::FloatingPreserve
                | ApplicationFullscreenExitRoute::MaximizedAllow
        )
}

pub(crate) fn fullscreen_rect_tolerance(scale_factor: f64) -> i32 {
    (scale_factor * 8.0).round().clamp(1.0, 20.0) as i32
}

pub(crate) fn rect_covers_monitor(rect: Rect, monitor: &MonitorInfo) -> bool {
    rect_matches_monitor(rect, monitor)
        || (rect.x <= monitor.rect.x
            && rect.y <= monitor.rect.y
            && rect.x.saturating_add(rect.width)
                >= monitor.rect.x.saturating_add(monitor.rect.width)
            && rect.y.saturating_add(rect.height)
                >= monitor.rect.y.saturating_add(monitor.rect.height))
}

fn rect_matches_monitor(rect: Rect, monitor: &MonitorInfo) -> bool {
    rects_match_with_tolerance(
        rect,
        monitor.rect,
        fullscreen_rect_tolerance(monitor.scale_factor),
    )
}

fn rects_match_with_tolerance(left: Rect, right: Rect, tolerance: i32) -> bool {
    (left.x - right.x).abs() <= tolerance
        && (left.y - right.y).abs() <= tolerance
        && (left.x.saturating_add(left.width) - right.x.saturating_add(right.width)).abs()
            <= tolerance
        && (left.y.saturating_add(left.height) - right.y.saturating_add(right.height)).abs()
            <= tolerance
}

pub(crate) fn chrome_rect_matches_layout_rect(
    chrome_rect: Rect,
    layout_rect: Rect,
    insets: (i32, i32, i32, i32),
    scale_factor: f64,
) -> bool {
    let (left, top, right, bottom) = insets;
    let visible_rect = Rect::new(
        chrome_rect.x.saturating_add(left),
        chrome_rect.y.saturating_add(top),
        chrome_rect.width.saturating_sub(left).saturating_sub(right),
        chrome_rect
            .height
            .saturating_sub(top)
            .saturating_sub(bottom),
    );
    let tolerance = fullscreen_rect_tolerance(scale_factor);
    (visible_rect.x - layout_rect.x).abs() <= tolerance
        && (visible_rect.y - layout_rect.y).abs() <= tolerance
        && (visible_rect.width - layout_rect.width).abs() <= tolerance
        && (visible_rect.height - layout_rect.height).abs() <= tolerance
}

pub(crate) fn should_ignore_hidden_still_visible(
    is_hidden_event: bool,
    is_managed: bool,
    is_visible: bool,
) -> bool {
    is_hidden_event && is_managed && is_visible
}

/// Editors put the config filename in a title or path; require separator boundaries
/// so a similarly named file cannot trigger a cross-workspace pull.
pub(crate) fn title_names_config_file(title: &str, filename: &str) -> bool {
    if filename.is_empty() {
        return false;
    }
    let title = title.to_lowercase();
    let filename = filename.to_lowercase();
    let sep = |b: u8| matches!(b, b' ' | b'\t' | b'-' | b'/' | b'\\');
    let bytes = title.as_bytes();
    title.match_indices(&filename).any(|(start, _)| {
        let before_ok = start == 0 || sep(bytes[start - 1]);
        let end = start + filename.len();
        let after_ok = end == bytes.len() || sep(bytes[end]);
        before_ok && after_ok
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElevationCheck {
    /// Window is manageable; any prior block record was cleared.
    Manageable,
    /// Newly blocked this session — caller should notify the user once.
    BlockedNew,
    /// Already known to be blocked — skip silently.
    BlockedKnown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionKind {
    Automatic,
    ExplicitReadmit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmitOutcome {
    Admitted,
    AlreadyManaged,
    AdmittedPlacementFailed,
    GatedIgnored,
    TransientSuppressed,
    NoWindowInfo,
    TopmostPopup,
    #[cfg_attr(test, allow(dead_code))]
    ShellCloaked,
    ElevationBlocked,
    TransientConsoleHost,
    PersistentIgnore,
    DialogLike,
    InsertFailed,
}

impl AppState {
    pub(crate) fn handle_window_event(&mut self, event: WindowEvent) {
        if let Some(hwnd) = match &event {
            WindowEvent::Created(hwnd, _)
            | WindowEvent::Destroyed(hwnd)
            | WindowEvent::Hidden(hwnd, _)
            | WindowEvent::Focused(hwnd, _)
            | WindowEvent::Minimized(hwnd, _)
            | WindowEvent::Restored(hwnd)
            | WindowEvent::MovedOrResized(hwnd)
            | WindowEvent::MoveSizeStart(hwnd)
            | WindowEvent::MoveSizeEnd(hwnd)
            | WindowEvent::TitleChanged(hwnd)
            | WindowEvent::MaximizedAdmissionRestored {
                window_id: hwnd, ..
            } => Some(*hwnd),
            _ => None,
        } {
            if leopardwm_platform_win32::focus_placeholder::is_focus_placeholder(hwnd) {
                return;
            }
        }

        let window_id = match &event {
            WindowEvent::Created(id, _)
            | WindowEvent::Destroyed(id)
            | WindowEvent::Hidden(id, _)
            | WindowEvent::Focused(id, _)
            | WindowEvent::Minimized(id, _)
            | WindowEvent::Restored(id)
            | WindowEvent::MovedOrResized(id)
            | WindowEvent::MoveSizeStart(id)
            | WindowEvent::MoveSizeEnd(id)
            | WindowEvent::TitleChanged(id)
            | WindowEvent::MaximizedAdmissionRestored { window_id: id, .. } => Some(*id),
            WindowEvent::DisplayChange
            | WindowEvent::WorkAreaChanged
            | WindowEvent::MouseEnterWindow(_)
            | WindowEvent::MouseLeftManaged => None,
        };

        // Destroyed/Hidden may name vanished windows; known windows also include test fixtures.
        if let Some(wid) = window_id {
            if !matches!(event, WindowEvent::Destroyed(_) | WindowEvent::Hidden(_, _))
                && !self.is_known_window(wid)
                && !leopardwm_platform_win32::is_valid_window(wid)
            {
                debug!("Ignoring event for invalid window {}", wid);
                return;
            }
        }

        match event {
            WindowEvent::Created(hwnd, event_time_ms) => {
                self.on_window_created(hwnd, event_time_ms)
            }
            WindowEvent::Destroyed(hwnd) => self.on_window_destroyed_or_hidden(hwnd, None),
            WindowEvent::Hidden(hwnd, event_time_ms) => {
                self.on_window_destroyed_or_hidden(hwnd, Some(event_time_ms))
            }
            WindowEvent::Focused(hwnd, event_time_ms) => {
                self.on_window_focused(hwnd, event_time_ms);
                self.raise_floating_above_tiled(hwnd);
            }
            WindowEvent::Minimized(hwnd, os_event_time_ms) => {
                self.on_window_minimized_from_event(hwnd, os_event_time_ms)
            }
            WindowEvent::Restored(hwnd) => self.on_window_restored(hwnd),
            WindowEvent::MaximizedAdmissionRestored {
                window_id,
                managed_lifetime_token,
                still_maximized,
            } => {
                self.on_maximized_admission_restored(
                    window_id,
                    managed_lifetime_token,
                    still_maximized,
                );
            }
            WindowEvent::MoveSizeStart(hwnd) => self.on_move_size_start(hwnd),
            WindowEvent::MoveSizeEnd(hwnd) => self.on_move_size_end(hwnd),
            WindowEvent::MovedOrResized(hwnd) => self.on_window_moved_or_resized(hwnd, false),
            WindowEvent::DisplayChange => self.on_display_change(),
            // Work-area changes normally reconcile through the debounced
            // DisplayChangeSettled path; handle direct dispatch defensively.
            WindowEvent::WorkAreaChanged => self.on_display_change(),
            WindowEvent::MouseEnterWindow(_) | WindowEvent::MouseLeftManaged => {
                // Both are handled by the main event loop's focus-follows-mouse
                // debouncing (schedule on enter, cancel on leave).
            }
            WindowEvent::TitleChanged(hwnd) => {
                if self.size_ignored_windows.remove(&hwnd) {
                    self.try_admit_window_at(hwnd, AdmissionKind::Automatic, None);
                }
                // Background title churn should not redraw strips. Labels are read from
                // lookup_window_info, so the event need not carry the new title.
                let in_visible_tabbed_column = self
                    .focused_workspace()
                    .map(|ws| {
                        ws.rows().iter().any(|r| {
                            r.columns()
                                .iter()
                                .any(|c| c.is_tabbed() && c.contains(hwnd))
                        })
                    })
                    .unwrap_or(false);
                if in_visible_tabbed_column {
                    self.update_tab_strip();
                }
            }
        }
    }

    fn prepare_admission_maximize_restore(
        &mut self,
        hwnd: u64,
        now: std::time::Instant,
        action: config::WindowAction,
        maximized_at_admission: bool,
        queue: &mut impl FnMut(u64) -> Result<bool, leopardwm_platform_win32::Win32Error>,
        is_maximized: &mut impl FnMut(u64) -> bool,
    ) {
        if action == config::WindowAction::Tile {
            if let Some(press_count) = self.native_user_press_count() {
                self.post_admission_maximize_restore_eligible
                    .insert(hwnd, (press_count, 0));
            }
        }
        if maximized_at_admission {
            self.queue_maximized_admission_restore(hwnd, now, queue, is_maximized);
        }
    }

    fn native_user_press_count(&self) -> Option<u64> {
        #[cfg(test)]
        {
            self.injected_press_count
        }
        #[cfg(not(test))]
        leopardwm_platform_win32::user_press_count()
    }

    fn queue_maximized_admission_restore(
        &mut self,
        hwnd: u64,
        now: std::time::Instant,
        queue: &mut impl FnMut(u64) -> Result<bool, leopardwm_platform_win32::Win32Error>,
        is_maximized: &mut impl FnMut(u64) -> bool,
    ) -> bool {
        match queue(hwnd) {
            Ok(will_report) => {
                if let Some((_, restores_issued)) =
                    self.post_admission_maximize_restore_eligible.get_mut(&hwnd)
                {
                    *restores_issued += 1;
                }
                if will_report {
                    self.pending_maximized_admission_restores.insert(hwnd);
                } else {
                    self.post_admission_maximize_restore_eligible.remove(&hwnd);
                }
                // Keep the zoom sampled at admission: the worker may already be
                // restoring the window, so a fresh read would race it.
                self.window_last_maximized_at.insert(hwnd, now);
                debug!("Queued maximized admission restore for {}", hwnd);
                true
            }
            Err(error) => {
                self.post_admission_maximize_restore_eligible.remove(&hwnd);
                debug!(
                    "Could not queue maximized admission restore for {}: {:?}",
                    hwnd, error
                );
                if is_maximized(hwnd) {
                    self.window_last_maximized_at.insert(hwnd, now);
                }
                false
            }
        }
    }

    fn try_restore_post_admission_maximize(
        &mut self,
        hwnd: u64,
        press_count: &mut impl FnMut() -> Option<u64>,
        queue: &mut impl FnMut(u64) -> Result<bool, leopardwm_platform_win32::Win32Error>,
        is_maximized: &mut impl FnMut(u64) -> bool,
    ) -> bool {
        if self.pending_maximized_admission_restores.contains(&hwnd) {
            return false;
        }
        let Some((admission_press_count, restores_issued)) = self
            .post_admission_maximize_restore_eligible
            .get(&hwnd)
            .copied()
        else {
            return false;
        };
        let now = std::time::Instant::now();
        if !self.window_managed_at.get(&hwnd).is_some_and(|managed_at| {
            now.saturating_duration_since(*managed_at) < SNAPBACK_SETTLE_AFTER_CREATE
        }) {
            self.post_admission_maximize_restore_eligible.remove(&hwnd);
            return false;
        }
        if press_count() != Some(admission_press_count) {
            self.post_admission_maximize_restore_eligible.remove(&hwnd);
            debug!(
                "User press or unavailable observers ended post-admission maximize restores for {}",
                hwnd
            );
            return false;
        }
        if restores_issued >= POST_ADMISSION_MAXIMIZE_RESTORE_BUDGET {
            self.post_admission_maximize_restore_eligible.remove(&hwnd);
            debug!(
                "Post-admission maximize restore budget exhausted for {}",
                hwnd
            );
            return false;
        }
        self.queue_maximized_admission_restore(hwnd, now, queue, is_maximized)
    }

    fn on_maximized_admission_restored(&mut self, hwnd: u64, token: u64, still_maximized: bool) {
        #[cfg(test)]
        let maximized = self.injected_window_maximized.get(&hwnd).copied();
        #[cfg(test)]
        let press_count = self.native_user_press_count();
        self.on_maximized_admission_restored_with_native_ops(
            hwnd,
            token,
            still_maximized,
            || {
                #[cfg(test)]
                {
                    press_count
                }
                #[cfg(not(test))]
                leopardwm_platform_win32::user_press_count()
            },
            leopardwm_platform_win32::queue_maximized_window_restore,
            |window_id| {
                #[cfg(test)]
                if let Some(maximized) = maximized {
                    return maximized;
                }
                leopardwm_platform_win32::is_window_maximized(window_id)
            },
        );
    }

    pub(crate) fn on_maximized_admission_restored_with_native_ops(
        &mut self,
        hwnd: u64,
        token: u64,
        worker_still_maximized: bool,
        mut press_count: impl FnMut() -> Option<u64>,
        mut queue: impl FnMut(u64) -> Result<bool, leopardwm_platform_win32::Win32Error>,
        mut is_maximized: impl FnMut(u64) -> bool,
    ) {
        if !self.is_managed_member(hwnd) || self.managed_lifetime_tokens.get(&hwnd) != Some(&token)
        {
            return;
        }
        self.pending_maximized_admission_restores.remove(&hwnd);
        if worker_still_maximized {
            self.window_last_maximized_at
                .insert(hwnd, std::time::Instant::now());
            return;
        }
        let still_maximized = is_maximized(hwnd);
        let managed_tiled = self
            .find_window_workspace(hwnd)
            .and_then(|(monitor_id, ws_idx)| self.workspaces.get(&monitor_id)?.get(ws_idx))
            .is_some_and(|workspace| {
                !workspace.is_floating(hwnd) && workspace.fullscreen_window_id() != Some(hwnd)
            });
        if self.is_application_fullscreen(hwnd) || !managed_tiled {
            self.post_admission_maximize_restore_eligible.remove(&hwnd);
        } else if still_maximized
            && self.try_restore_post_admission_maximize(
                hwnd,
                &mut press_count,
                &mut queue,
                &mut is_maximized,
            )
        {
            return;
        }
        if still_maximized {
            self.window_last_maximized_at
                .insert(hwnd, std::time::Instant::now());
        } else {
            self.window_last_maximized_at.remove(&hwnd);
            self.last_placed_layout_rects.remove(&hwnd);
            if let Err(error) = self.apply_layout() {
                warn!(
                    "Failed to apply layout after maximized admission restore: {}",
                    error
                );
            }
        }
    }

    /// Remembered widths belong to a managed lifetime, not a reusable HWND.
    /// Consume the entry and expire old entries to keep the map bounded.
    pub(crate) fn take_remembered_column_width(&mut self, hwnd: u64) -> Option<i32> {
        self.hidden_column_widths
            .retain(|_, entry| entry.hidden_at.elapsed() < RECENTLY_HIDDEN_TTL);
        let entry = self.hidden_column_widths.remove(&hwnd)?;
        if self.recently_hidden_names_current_lifetime(entry.managed_token, hwnd) {
            Some(entry.width)
        } else {
            None
        }
    }

    /// Changed titles or block reasons get a fresh notification; identical records
    /// stay deduped until management becomes possible or the window departs.
    pub(crate) fn note_elevation_block(
        &mut self,
        hwnd: u64,
        title: &str,
        block: leopardwm_platform_win32::ManageBlock,
    ) -> ElevationCheck {
        if !block.is_blocked() {
            // A recycled HWND may belong to a manageable window.
            self.elevation_blocked.remove(&hwnd);
            return ElevationCheck::Manageable;
        }
        let record = ElevationBlockedRecord {
            title: title.to_string(),
            reason: block,
        };
        match self.elevation_blocked.insert(hwnd, record) {
            None => ElevationCheck::BlockedNew,
            Some(prev) if prev.title != title || prev.reason != block => ElevationCheck::BlockedNew,
            Some(_) => ElevationCheck::BlockedKnown,
        }
    }

    /// Returns true if the window owned by `pid` cannot be managed because
    /// Windows UIPI blocks the non-elevated daemon from repositioning a
    /// higher-integrity window. Records it (so it shows in `lwm doctor` and
    /// stays skipped) and toasts the user once per window. Shared by the
    /// live-create and the startup/refresh enumerate paths so the skip behaves
    /// identically in both.
    #[cfg(not(test))]
    pub(crate) fn skip_if_elevation_blocked(
        &mut self,
        hwnd: u64,
        pid: u32,
        title: &str,
        class_name: &str,
    ) -> bool {
        use leopardwm_platform_win32::ManageBlock;
        let block = leopardwm_platform_win32::manage_block(pid);
        match self.note_elevation_block(hwnd, title, block) {
            ElevationCheck::Manageable => false,
            ElevationCheck::BlockedNew => {
                warn!(
                    "Cannot tile window '{}' ({}): {:?} — leaving it floating and ignoring it \
                     for this session.",
                    title, class_name, block
                );
                // Tailor remediation: elevating helps for a higher-integrity
                // window, but not for a protected/PPL process.
                let key = if matches!(block, ManageBlock::Protected) {
                    "notification.floating.protected"
                } else {
                    "notification.floating.elevated"
                };
                let language = &self.config.appearance.language;
                let body = crate::locale::format(language, key, &[("title", title)]);
                crate::notify::show_toast(
                    &crate::locale::text(language, "notification.floating.title"),
                    &body,
                );
                true
            }
            ElevationCheck::BlockedKnown => {
                debug!("Window {} still blocked by privilege level, ignoring", hwnd);
                true
            }
        }
    }

    pub(crate) fn elevation_blocks_admission(
        &mut self,
        hwnd: u64,
        pid: u32,
        title: &str,
        class_name: &str,
    ) -> bool {
        #[cfg(test)]
        {
            let _ = (pid, class_name);
            if let Some(block) = self.injected_manage_block.get(&hwnd).copied() {
                return !matches!(
                    self.note_elevation_block(hwnd, title, block),
                    ElevationCheck::Manageable
                );
            }
            false
        }
        #[cfg(not(test))]
        self.skip_if_elevation_blocked(hwnd, pid, title, class_name)
    }

    /// Re-assert the fullscreen window `fs_wid` as the focused and foreground
    /// window, after something tried to put another window in front of it (a new
    /// window opening, or a window self-activating behind it). The Win32 raise is
    /// always attempted (the visual fix); internal focus and the IPC broadcast
    /// follow only if the window is still in a workspace. The border stays hidden:
    /// monocle fullscreen has no focus ring, matching `focus_in_fullscreen`.
    fn reassert_fullscreen_focus(&mut self, fs_wid: u64) {
        if let Err(e) = leopardwm_platform_win32::set_foreground_window(fs_wid) {
            debug!(
                "Could not raise fullscreen window {} to the top: {:?}",
                fs_wid, e
            );
        }
        if let Some((mid, widx)) = self.find_window_workspace(fs_wid) {
            let refocused = self
                .workspaces
                .get_mut(&mid)
                .and_then(|v| v.get_mut(widx))
                .is_some_and(|ws| ws.focus_window(fs_wid).is_ok());
            if refocused {
                self.previous_focused_hwnd = Some(fs_wid);
                self.hide_border();
                self.broadcast_focused_window_if_changed(mid as i64, Some(fs_wid));
            }
        }
    }

    fn on_window_created(&mut self, hwnd: u64, event_time_ms: u32) {
        let _ = self.try_admit_window_at(hwnd, AdmissionKind::Automatic, Some(event_time_ms));
    }

    /// Focus recovery and an unmanaged restore are not Create/Show events.
    /// They still admit, but a later Hidden must depart: no guard time is recorded.
    fn admit_created_without_event_time(&mut self, hwnd: u64) {
        if !self.is_known_window(hwnd) && !leopardwm_platform_win32::is_valid_window(hwnd) {
            debug!("Ignoring event for invalid window {}", hwnd);
            return;
        }
        let _ = self.try_admit_window(hwnd, AdmissionKind::Automatic);
    }

    /// Suppression belongs to the hidden lifetime. A recycled handle or a window
    /// without popup shape must not inherit it; an unrecorded lifetime keeps suppression.
    fn suppressed_transient_popup(&mut self, hwnd: u64) -> bool {
        let Some(entry) = self.recently_hidden_hwnds.get(&hwnd).copied() else {
            return false;
        };
        if entry.hidden_at.elapsed() >= RECENTLY_HIDDEN_TTL {
            return false;
        }
        if !self.recently_hidden_names_current_lifetime(entry.managed_token, hwnd) {
            self.recently_hidden_hwnds.remove(&hwnd);
            return false;
        }
        // Only suppress genuinely popup-shaped re-creations (the Electron
        // notification toasts this guard exists for). A real window the
        // user dismissed quickly (e.g. Edge's download popup) keeps a
        // caption/minimize box; suppressing it would leave it floating,
        // untracked and overlaying the layout, for the whole TTL. Tests
        // inject synthetic HWNDs with no real window style, so the shape
        // check is production-only and suppression stays unconditional
        // under cfg(test).
        #[cfg(not(test))]
        let is_popup = leopardwm_platform_win32::is_frameless_popup(hwnd);
        #[cfg(test)]
        let is_popup = true;
        if is_popup {
            debug!(
                "Ignoring transient re-created popup {} (hidden {}ms ago)",
                hwnd,
                entry.hidden_at.elapsed().as_millis()
            );
            return true;
        }
        self.recently_hidden_hwnds.remove(&hwnd);
        false
    }

    /// The suppression entry still names this HWND. A recycled handle drops it
    /// and is not recovered as the window that was hidden.
    fn same_lifetime_recently_hidden(&mut self, hwnd: u64) -> bool {
        let Some(entry) = self.recently_hidden_hwnds.get(&hwnd).copied() else {
            return false;
        };
        if self.recently_hidden_names_current_lifetime(entry.managed_token, hwnd) {
            return true;
        }
        self.recently_hidden_hwnds.remove(&hwnd);
        false
    }

    pub(crate) fn try_admit_window(&mut self, hwnd: u64, kind: AdmissionKind) -> AdmitOutcome {
        self.try_admit_window_at(hwnd, kind, None)
    }

    fn try_admit_window_at(
        &mut self,
        hwnd: u64,
        kind: AdmissionKind,
        admitted_at_event_ms: Option<u32>,
    ) -> AdmitOutcome {
        self.try_admit_window_at_with_native_ops(
            hwnd,
            kind,
            admitted_at_event_ms,
            leopardwm_platform_win32::is_window_maximized,
            leopardwm_platform_win32::queue_maximized_window_restore,
        )
    }

    pub(crate) fn try_admit_window_at_with_native_ops(
        &mut self,
        hwnd: u64,
        kind: AdmissionKind,
        admitted_at_event_ms: Option<u32>,
        is_maximized: impl FnMut(u64) -> bool,
        queue_maximized_restore: impl FnMut(u64) -> Result<bool, leopardwm_platform_win32::Win32Error>,
    ) -> AdmitOutcome {
        // Depart before the body. Its own duplicate check then sees a non-member
        // and does not sample foreground a second time. Reconcile only a real
        // replaced departure: an ordinary Created must not touch tracked focus.
        self.size_ignored_windows.remove(&hwnd);
        let replaced = self.depart_replaced_managed_lifetime(hwnd);
        let outcome = self.admit_window_after_replaced_departure(
            hwnd,
            kind,
            admitted_at_event_ms,
            is_maximized,
            queue_maximized_restore,
        );
        if replaced {
            self.reconcile_replaced_lifetime_admission(hwnd);
        }
        outcome
    }

    #[allow(clippy::too_many_lines)]
    fn admit_window_after_replaced_departure(
        &mut self,
        hwnd: u64,
        kind: AdmissionKind,
        admitted_at_event_ms: Option<u32>,
        mut is_maximized: impl FnMut(u64) -> bool,
        mut queue_maximized_restore: impl FnMut(
            u64,
        )
            -> Result<bool, leopardwm_platform_win32::Win32Error>,
    ) -> AdmitOutcome {
        // Recycle departs before suppression and the ignore gate. A cloak Hidden
        // can mark this HWND transient, and that entry must not reject the replacement.
        if self.duplicate_managed_admission(hwnd) {
            return AdmitOutcome::AlreadyManaged;
        }

        // Suppress transient windows that rapidly show/hide the same HWND
        // (e.g., Electron notification popups from Beeper, Slack).
        if kind == AdmissionKind::Automatic && self.suppressed_transient_popup(hwnd) {
            return AdmitOutcome::TransientSuppressed;
        }
        self.recently_hidden_hwnds
            .retain(|_, entry| entry.hidden_at.elapsed() < RECENTLY_HIDDEN_TTL);

        if kind == AdmissionKind::Automatic
            && self.temporary_ignore_gate(hwnd) == crate::temporary_ignore::IgnoreGate::Block
        {
            return AdmitOutcome::GatedIgnored;
        }

        let Some(win_info) = self.lookup_window_info(hwnd) else {
            return AdmitOutcome::NoWindowInfo;
        };
        if leopardwm_platform_win32::is_excluded_topmost_popup_hwnd(hwnd) {
            debug!("Ignoring topmost notification popup {}", hwnd);
            return AdmitOutcome::TopmostPopup;
        }
        {
            // Skip shell-cloaked windows (suspended UWP frames, windows
            // on other virtual desktops). These are valid HWNDs with
            // WS_VISIBLE but no rendered content.
            #[cfg(not(test))]
            if leopardwm_platform_win32::is_window_shell_cloaked(hwnd) {
                debug!(
                    "Ignoring shell-cloaked window: {} ({})",
                    win_info.title, win_info.class_name
                );
                return AdmitOutcome::ShellCloaked;
            }

            // Windows UIPI blocks a non-elevated daemon from repositioning an
            // elevated window: SetWindowPos is silently refused, so tiling it
            // would reserve a column the window never occupies. Leave it
            // floating where the OS placed it and ignore it for the session.
            if self.elevation_blocks_admission(
                hwnd,
                win_info.process_id,
                &win_info.title,
                &win_info.class_name,
            ) {
                return AdmitOutcome::ElevationBlocked;
            }

            let executable = get_process_executable(win_info.process_id).unwrap_or_default();

            // Console hosts briefly use an executable-path title before setting a real
            // one. Scheduled-task windows can disappear during that interval; admitting
            // them would cause two pointless layout reflows. Focus recovery retries
            // interactive consoles once they have a real title.
            if is_transient_console_host(&win_info.title, &win_info.class_name, &executable) {
                debug!(
                    "Skipping transient console-host window with exe-path title: {} ({})",
                    win_info.title, win_info.class_name
                );
                return AdmitOutcome::TransientConsoleHost;
            }

            // Match once: the action and the per-app open extras both come from
            // the same first-matching rule (or the defaults when none matches).
            let matched = self.admission_rule(&win_info, &executable);
            let rule_matched = matched.is_some();
            let action = matched
                .map(|r| r.action)
                .unwrap_or(config::WindowAction::Tile);
            let (
                rule_workspace,
                rule_maximized,
                rule_column_width,
                rule_slot,
                rule_row,
                rule_sticky,
            ) = matched
                .map(|r| {
                    (
                        r.open_on_workspace,
                        r.open_maximized,
                        r.column_width,
                        r.open_in_column,
                        r.row,
                        r.sticky,
                    )
                })
                .unwrap_or((None, false, None, None, None, false));

            if action == config::WindowAction::Ignore {
                debug!(
                    "Ignoring window by rule: {} ({})",
                    win_info.title, win_info.class_name
                );
                return AdmitOutcome::PersistentIgnore;
            }

            if let Some(kind) = self.unmanaged_helper_kind(&win_info, rule_matched) {
                debug!(
                    "Leaving {} window unmanaged: {} ({})",
                    kind, win_info.title, win_info.class_name
                );
                return AdmitOutcome::DialogLike;
            }

            let recreated_slot =
                self.take_recreated_window_slot(&win_info, kind, action, rule_sticky);
            let opening_monitor = self
                .monitors
                .values()
                .find(|monitor| monitor.contains_rect_center(&win_info.rect))
                .map(|monitor| monitor.id)
                .unwrap_or(self.focused_monitor);
            let preferred_monitor = self
                .preferred_new_window_monitor(rule_sticky)
                .filter(|_| kind == AdmissionKind::Automatic);
            let monitor_id = recreated_slot
                .as_ref()
                .map(|slot| slot.monitor)
                .unwrap_or_else(|| preferred_monitor.unwrap_or(opening_monitor));

            let floating_rect = (action == config::WindowAction::Float).then(|| {
                self.floating_rect_for_admission(
                    &win_info,
                    &executable,
                    monitor_id,
                    recreated_slot.is_none() && preferred_monitor.is_some(),
                )
            });

            let viewport_width = self.viewport_width_for(monitor_id);

            let active_idx = self.active_workspace_idx(monitor_id);
            // A sticky window shows on every workspace, so it always opens on the
            // active one; an open_on_workspace would only hide it until a switch.
            // Explicit readmit always uses the active workspace of the native monitor.
            let target_idx = if let Some(slot) = &recreated_slot {
                slot.workspace
            } else if rule_sticky || kind == AdmissionKind::ExplicitReadmit {
                active_idx
            } else {
                rule_workspace.unwrap_or(active_idx)
            };
            let opens_in_background =
                kind != AdmissionKind::ExplicitReadmit && target_idx != active_idx;
            if opens_in_background {
                self.ensure_workspace_exists(monitor_id, target_idx);
            }

            // Snapshot before structural change for tiled window
            // animation. A background open doesn't change the active
            // layout, so no transition is needed.
            let snapshot = if action == config::WindowAction::Tile && !opens_in_background {
                Some(self.snapshot_layout())
            } else {
                None
            };

            // Automatic re-admission preserves the hidden window's width; explicit
            // readmission instead reapplies the rule's viewport fraction.
            let rule_width_px = if kind == AdmissionKind::ExplicitReadmit {
                rule_column_width.map(|f| ((f * f64::from(viewport_width)).round() as i32).max(100))
            } else {
                self.take_remembered_column_width(hwnd).or_else(|| {
                    rule_column_width
                        .map(|f| ((f * f64::from(viewport_width)).round() as i32).max(100))
                })
            };
            let take_workspace_focus = if recreated_slot.is_some() {
                self.config.behavior.focus_new_windows && !opens_in_background
            } else {
                kind == AdmissionKind::ExplicitReadmit
                    || self.config.behavior.focus_new_windows
                    || opens_in_background
            };
            let native_maximized_at_admission =
                action == config::WindowAction::Tile && is_maximized(hwnd);

            if let Some(workspace) = self
                .workspaces
                .get_mut(&monitor_id)
                .and_then(|v| v.get_mut(target_idx))
            {
                let added = match action {
                    config::WindowAction::Float => {
                        let rect = floating_rect.unwrap_or_else(|| {
                            let viewport = self
                                .monitors
                                .get(&monitor_id)
                                .map(|m| m.work_area)
                                .unwrap_or_else(|| {
                                    Rect::new(
                                        0,
                                        0,
                                        FALLBACK_VIEWPORT_WIDTH,
                                        FALLBACK_VIEWPORT_HEIGHT,
                                    )
                                });
                            Rect::new(
                                viewport.x + (viewport.width - 800) / 2,
                                viewport.y + (viewport.height - 600) / 2,
                                800,
                                600,
                            )
                        });
                        workspace.add_floating(hwnd, rect).is_ok()
                    }
                    config::WindowAction::Tile => {
                        let in_column = self.config.behavior.new_window_placement
                            == config::NewWindowPlacement::InColumn
                            && workspace.column_count() > 0;
                        let ok = insert_admitted_tile(
                            workspace,
                            hwnd,
                            rule_width_px,
                            rule_slot,
                            rule_row,
                            in_column,
                            take_workspace_focus,
                            recreated_slot.as_ref(),
                        );
                        if ok
                            && recreated_slot.is_none()
                            && (rule_maximized || native_maximized_at_admission)
                            && workspace.focused_window() == Some(hwnd)
                        {
                            workspace.maximize_focused_column(viewport_width);
                        }
                        ok
                    }
                    config::WindowAction::Ignore => unreachable!(),
                };

                if added {
                    let now = std::time::Instant::now();
                    self.window_managed_at.insert(hwnd, now);
                    info!(
                        "Window created: {} ({}) - added to monitor {} workspace {} as {:?}",
                        win_info.title,
                        win_info.class_name,
                        monitor_id,
                        target_idx + 1,
                        action
                    );
                    // A floating sticky pins to overlay every workspace; a tiled
                    // one stays tiled and follows as a column on each switch.
                    if rule_sticky {
                        if matches!(action, config::WindowAction::Float) {
                            workspace.set_floating_pinned(hwnd, true);
                        }
                        self.sticky_windows.insert(hwnd);
                    }
                    if kind == AdmissionKind::Automatic
                        && self.config.behavior.focus_new_windows
                        && !opens_in_background
                    {
                        self.focused_monitor = monitor_id;
                        if matches!(action, config::WindowAction::Float) {
                            self.previous_focused_hwnd = Some(hwnd);
                        }
                        workspace.ensure_focused_visible_animated(viewport_width);
                    } else if kind == AdmissionKind::ExplicitReadmit {
                        workspace.ensure_focused_visible_animated(viewport_width);
                    }
                    self.record_managed_lifetime(hwnd, admitted_at_event_ms);
                    self.prepare_admission_maximize_restore(
                        hwnd,
                        now,
                        action,
                        native_maximized_at_admission,
                        &mut queue_maximized_restore,
                        &mut is_maximized,
                    );
                    self.record_managed_window_identity(&win_info);
                    if recreated_slot.is_some() {
                        if opens_in_background {
                            self.arm_background_rejoin_activation(hwnd, admitted_at_event_ms);
                        }
                        info!("Window {} rejoined its pre-sleep column or tab on monitor {} workspace {}", hwnd, monitor_id, target_idx + 1);
                    }
                    if opens_in_background {
                        #[cfg(not(test))]
                        if !self.paused {
                            let _ = leopardwm_platform_win32::move_window_offscreen(hwnd);
                        }
                        self.sync_inactive_workspace_taskbar_button(hwnd);
                    }
                    if let Some(snapshot) = snapshot {
                        self.start_layout_transition(snapshot);
                    }
                    if matches!(action, config::WindowAction::Tile) {
                        self.disable_snap_for_window(hwnd);
                    }
                    let layout_failed = self
                        .apply_layout()
                        .inspect_err(|e| {
                            warn!("Failed to apply layout after window create: {}", e);
                        })
                        .is_err();
                    // In fullscreen the other tiled windows are hidden only by
                    // the fullscreen window sitting on top of them (cloaking an
                    // external window is a no-op), so a tiled window Windows just
                    // raised on creation renders over the fullscreen one. Keep
                    // monocle behavior: the new window joins the layout behind,
                    // and the fullscreen window stays focused and on top until
                    // the user leaves fullscreen. (A floating window is meant to
                    // overlay, and a background one is parked off-screen, so this
                    // is tiled-and-active only.)
                    let keep_fullscreen_on_top =
                        if matches!(action, config::WindowAction::Tile) && !opens_in_background {
                            self.workspaces
                                .get(&monitor_id)
                                .and_then(|workspaces| workspaces.get(active_idx))
                                .filter(|ws| ws.is_fullscreen())
                                .and_then(|ws| ws.fullscreen_window_id())
                                .filter(|&fs| fs != hwnd)
                        } else {
                            None
                        };
                    // Skip the newcomer's foreground sync when we're about to
                    // re-raise the fullscreen window, to avoid a double focus
                    // transition.
                    if kind == AdmissionKind::Automatic
                        && self.config.behavior.focus_new_windows
                        && !opens_in_background
                        && keep_fullscreen_on_top.is_none()
                    {
                        self.sync_foreground_window();
                    }
                    if kind == AdmissionKind::Automatic {
                        if let Some(fs_wid) = keep_fullscreen_on_top {
                            if self.config.behavior.focus_new_windows
                                || monitor_id == self.focused_monitor
                            {
                                self.reassert_fullscreen_focus(fs_wid);
                            } else if let Err(e) =
                                leopardwm_platform_win32::raise_window_no_activate(fs_wid)
                            {
                                debug!(
                                    "Could not raise fullscreen window {} without activation: {:?}",
                                    fs_wid, e
                                );
                            }
                        }
                    }
                    return if layout_failed {
                        AdmitOutcome::AdmittedPlacementFailed
                    } else {
                        AdmitOutcome::Admitted
                    };
                } else {
                    debug!("Failed to add window {} to workspace", hwnd);
                    return AdmitOutcome::InsertFailed;
                }
            }
            AdmitOutcome::InsertFailed
        }
    }

    /// Hidden carries a WinEvent timestamp; Destroyed does not.
    fn on_window_destroyed_or_hidden(&mut self, hwnd: u64, hidden_at_ms: Option<u32>) {
        let is_hidden_event = hidden_at_ms.is_some();
        if !is_hidden_event && self.destroyed_names_current_lifetime(hwnd) {
            debug!(
                "Ignoring stale Destroyed for live hwnd {} (current lifetime still present)",
                hwnd
            );
            self.on_temporary_ignore_destroyed(hwnd);
            return;
        }
        // A Hidden strictly earlier than this member's Create/Show time names an
        // older lifetime. Eventless admissions record no time, so Hidden departs.
        if let Some(event_time_ms) = hidden_at_ms {
            if let Some(admitted_at_ms) = self.admitted_event_time_ms_if_current_member(hwnd) {
                if !event_time_is_no_later_than(admitted_at_ms, event_time_ms) {
                    debug!(
                        "Ignoring stale Hidden for hwnd {} (event {} before admission {})",
                        hwnd, event_time_ms, admitted_at_ms
                    );
                    return;
                }
            }
        }
        self.depart_destroyed_or_hidden_window(hwnd, is_hidden_event, DepartureCause::Event);
    }

    /// Replaced lifetimes depart before admission, bypassing stale-event guards.
    /// The foreground HWND may already be the replacement: clear tracked focus
    /// without treating that handle as the departed window, then let admission reconcile it.
    pub(crate) fn depart_destroyed_or_hidden_window(
        &mut self,
        hwnd: u64,
        is_hidden_event: bool,
        cause: DepartureCause,
    ) {
        let event_name = if is_hidden_event {
            "hidden"
        } else {
            "destroyed"
        };

        // A stashed scratchpad window lives outside all workspaces and
        // is cloaked, so only a real destroy (not a spurious Hidden
        // from cloaking) should clear its designation.
        if !is_hidden_event {
            self.scratchpad_on_window_destroyed(hwnd);
            self.sticky_on_window_destroyed(hwnd);
            self.on_temporary_ignore_destroyed(hwnd);
            // Forget any remembered floating focus for this window so
            // a recycled HWND can't wrongly re-focus on workspace return.
            self.floating_focus.retain(|_, &mut h| h != hwnd);
            // Drop the cached window icon: the HICON dies with its
            // window, and a recycled HWND must re-probe.
            self.overview_icon_cache.remove(&hwnd);
            // Drop it from the taskbar hidden set so a recycled HWND isn't
            // skipped by the hide change-gate.
            leopardwm_platform_win32::taskbar::taskbar_forget(hwnd);
            // Forget an elevation-blocked window once it's truly gone, so a
            // recycled HWND for a normal window isn't wrongly skipped.
            self.elevation_blocked.remove(&hwnd);
            // Hidden keeps a remembered width for a later show. Destroy must
            // not, or the next occupant of this HWND inherits it.
            self.hidden_column_widths.remove(&hwnd);
            // Drop any remembered move-back origin so a recycled HWND doesn't
            // inherit a stale column restore.
            self.move_origins.remove(&hwnd);
            // This window may have anchored another's restore; drop the stale
            // sibling so a recycled HWND can't redirect a move-back to the wrong
            // column (it falls back to the remembered index instead).
            for origin in self.move_origins.values_mut() {
                if origin.sibling == Some(hwnd) {
                    origin.sibling = None;
                }
            }
            // Drop any size-violation suspect state so the map stays bounded and
            // a recycled HWND doesn't inherit it.
            leopardwm_platform_win32::clear_suspected_oversize(hwnd);
            // Scrub a window that dies while its monitor is stashed (disconnected),
            // so it isn't resurrected as a ghost column when the monitor returns.
            for layout in self.stashed_monitor_layouts.values_mut() {
                for ws in &mut layout.workspaces {
                    let _ = ws.remove_window(hwnd);
                    ws.remove_floating(hwnd);
                }
            }
            // Drop a stash emptied by window deaths so the map stays bounded when
            // a disconnected monitor never returns.
            self.stashed_monitor_layouts.retain(|_, layout| {
                layout
                    .workspaces
                    .iter()
                    .any(|ws| ws.window_count() > 0 || !ws.floating_windows().is_empty())
            });
        }

        // For Hidden events, verify the window is actually gone.
        // Electron apps (Slack, Beeper, Obsidian) fire spurious
        // EVENT_OBJECT_HIDE on their main window during internal
        // state changes (notification badges, focus between panes).
        // If the HWND is still valid and visible, ignore the event.
        let is_managed = self.find_window_workspace(hwnd).is_some()
            || self
                .drag_state
                .as_ref()
                .is_some_and(|drag| drag.hwnd == hwnd && drag.is_tiled);
        if should_ignore_hidden_still_visible(
            is_hidden_event,
            is_managed,
            self.is_window_visible_now(hwnd),
        ) {
            debug!(
                "Ignoring spurious Hidden event for still-visible window {}",
                hwnd
            );
            return;
        }
        if self
            .pending_workspace_switch_focus
            .is_some_and(|intent| intent.source_hwnd == hwnd)
        {
            self.pending_workspace_switch_focus = None;
        }

        let matching_drag = self.drag_state.as_ref().filter(|d| d.hwnd == hwnd);
        let drag_source = matching_drag.map(|d| (d.source_monitor, d.source_workspace_idx));
        let layout_home = self.find_window_workspace(hwnd).or(drag_source);
        let snapshot = layout_home.map(|_| self.snapshot_layout());

        let cancel = self.cancel_matching_unfinished_move_size_ui(hwnd);
        self.release_departing_hwnd_ghost(hwnd);

        self.last_placed_layout_rects.remove(&hwnd);
        self.deferred_moved_or_resized.remove(&hwnd);
        self.deferred_physical_landings.remove(&hwnd);
        self.size_ignored_windows.remove(&hwnd);
        self.window_move_recheck_attempts.remove(&hwnd);
        leopardwm_platform_win32::forget_offscreen_placement(hwnd);
        self.clear_physical_window_state(hwnd);
        self.application_fullscreen.remove(&hwnd);

        leopardwm_platform_win32::snapshot::snapshot_remove(hwnd);

        // Drop any tab title override too — both Destroyed and
        // Hidden imply the window is no longer in any tabbed
        // column. Without this, hidden-but-not-destroyed apps
        // (minimize-to-tray patterns) would accumulate stale
        // overrides indefinitely in the persisted state.
        self.tab_title_overrides.remove(&hwnd);
        self.window_last_maximized_at.remove(&hwnd);

        // A short-lived Hidden suppresses a later popup on this HWND.
        // Long-lived windows (e.g., close-to-tray apps) are allowed to re-tile.
        // Cloaking a stashed scratchpad can emit Hidden while it is still the
        // same window. A shown scratchpad is an ordinary floating member, and
        // a real Destroyed still drops the record.
        let recreated_donation = if is_hidden_event && cause == DepartureCause::Event {
            self.prepare_recreated_window_slot(hwnd)
        } else {
            None
        };
        let stashed_scratchpad_hidden = is_hidden_event
            && crate::managed_lifetime::is_stashed_scratchpad(self.scratchpad, hwnd);
        let recorded_lifetime = if stashed_scratchpad_hidden {
            self.managed_lifetime_tokens.get(&hwnd).copied()
        } else {
            self.take_managed_lifetime_token(hwnd)
        };
        // Only a short-lived Hidden marks the HWND transient. A real Destroyed
        // ends that lifetime: it must not create an entry, and it drops one a
        // prior Hidden left behind so a recycled popup is not suppressed.
        let managed_at = self.window_managed_at.remove(&hwnd);
        if is_hidden_event {
            if let Some(managed_at) = managed_at {
                if managed_at.elapsed() < TRANSIENT_WINDOW_THRESHOLD {
                    debug!(
                        "Marking window {} as transient (managed {}ms)",
                        hwnd,
                        managed_at.elapsed().as_millis()
                    );
                    self.recently_hidden_hwnds.insert(
                        hwnd,
                        RecentlyHiddenEntry {
                            hidden_at: std::time::Instant::now(),
                            managed_token: self
                                .managed_token_for_hidden_record(hwnd, recorded_lifetime),
                        },
                    );
                } else {
                    debug!(
                        "Window {} was managed {}s, not marking as transient",
                        hwnd,
                        managed_at.elapsed().as_secs()
                    );
                }
            }
        } else {
            self.recently_hidden_hwnds.remove(&hwnd);
        }
        self.recently_hidden_hwnds
            .retain(|_, entry| entry.hidden_at.elapsed() < RECENTLY_HIDDEN_TTL);

        // Sample before clearing. A replaced lifetime whose foreground is this
        // HWND still drops tracked focus; admission adopts it only if it lands
        // on the selected workspace.
        let was_tracked_focus = self.previous_focused_hwnd == Some(hwnd);
        let decision = self.departing_focus_decision_for(hwnd, was_tracked_focus, cause);
        if was_tracked_focus {
            self.hide_border();
            self.previous_focused_hwnd = None;
            let monitor = self.focused_monitor as i64;
            self.broadcast_focused_window_if_changed(monitor, None);
        }

        let mut was_tiled = false;
        if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
            let viewport_width = self.viewport_width_for(monitor_id);
            // Remember the column width before removal so a hidden-then-reshown
            // window (e.g. a virtual-desktop tool that hides/shows on switch)
            // re-tiles at its prior width instead of the default.
            let mut hidden_width: Option<i32> = None;
            if let Some(workspace) = self
                .workspaces
                .get_mut(&monitor_id)
                .and_then(|v| v.get_mut(ws_idx))
            {
                if is_hidden_event {
                    hidden_width =
                        workspace
                            .find_window_location_rc(hwnd)
                            .and_then(|(row, col, _)| {
                                workspace.column_for_row(row, col).map(|c| c.width())
                            });
                }
                let was_floating = workspace.remove_floating(hwnd);

                if was_floating {
                    info!(
                        "Floating window {} {} - removed from monitor {}",
                        hwnd, event_name, monitor_id
                    );
                } else if let Err(e) = workspace.remove_window(hwnd) {
                    warn!("Failed to remove window {}: {}", hwnd, e);
                } else {
                    was_tiled = true;
                    info!(
                        "Window {} {} - removed from monitor {}",
                        hwnd, event_name, monitor_id
                    );
                    workspace.ensure_focused_visible_animated(viewport_width);
                }
            }
            if was_tiled {
                if let Some(w) = hidden_width {
                    self.hidden_column_widths.insert(
                        hwnd,
                        HiddenColumnWidth {
                            hidden_at: std::time::Instant::now(),
                            width: w,
                            managed_token: self
                                .managed_token_for_hidden_record(hwnd, recorded_lifetime),
                        },
                    );
                }
            }
            self.hidden_column_widths
                .retain(|_, entry| entry.hidden_at.elapsed() < RECENTLY_HIDDEN_TTL);
            // Restore WS_MAXIMIZEBOX (no-op if not tracked)
            self.restore_snap_for_window(hwnd);
        } else if cancel.removed_from_source {
            if let Some((monitor_id, ws_idx)) = drag_source {
                let viewport_width = self.viewport_width_for(monitor_id);
                if let Some(workspace) = self
                    .workspaces
                    .get_mut(&monitor_id)
                    .and_then(|v| v.get_mut(ws_idx))
                {
                    workspace.ensure_focused_visible_animated(viewport_width);
                }
            }
        }

        if was_tiled {
            self.donate_recreated_window_slot(recreated_donation);
        }

        if let Some(snapshot) = snapshot {
            if was_tiled || cancel.needs_layout() {
                let mut snapshot = snapshot;
                snapshot.remove(&hwnd);
                snapshot.remove(&crate::state::DRAG_PLACEHOLDER_HWND);
                if self.start_layout_transition(snapshot) && decision.suppress_landing_resync {
                    if let Some(ref mut transition) = self.layout_transition {
                        transition.suppress_landing_focus_resync = true;
                    }
                }
                if let Err(e) = self.apply_layout() {
                    warn!("Failed to apply layout after window {}: {}", event_name, e);
                }
            }
        }

        let emptied_selected = layout_home.is_some_and(|(monitor_id, ws_idx)| {
            monitor_id == self.focused_monitor
                && ws_idx == self.active_workspace_idx(monitor_id)
                && self
                    .workspaces
                    .get(&monitor_id)
                    .and_then(|workspaces| workspaces.get(ws_idx))
                    .is_some_and(workspace_is_genuinely_empty)
        });
        if emptied_selected {
            // Empty selection: clear logical focus/border even when tracking
            // already names another HWND. The same-HWND Focused early-return
            // would otherwise bypass the guard. A replaced lifetime is not a
            // close, so it must not arm that guard.
            self.clear_logical_focus_for_empty_selection();
            if cause != DepartureCause::ReplacedLifetime {
                self.arm_pending_last_window_departure(
                    decision.replacement_hwnd,
                    self.event_time_now_ms(),
                    LastWindowDepartureOrigin::DirectDestroyedOrHidden,
                );
            }
        } else if decision.recover {
            self.sync_foreground_window();
        } else if was_tracked_focus {
            if let Some(replacement) = decision.replacement_hwnd {
                if !self.adopt_managed_replacement_without_stealing_focus(replacement) {
                    self.reconcile_border_without_stealing_focus_for(Some(replacement));
                }
            }
        } else {
            self.reconcile_border_without_stealing_focus_for(decision.replacement_hwnd);
        }
    }

    fn is_window_visible_now(&self, hwnd: u64) -> bool {
        #[cfg(test)]
        if self.injected_visible_hwnds.contains(&hwnd) {
            return true;
        }
        leopardwm_platform_win32::is_window_visible(hwnd)
    }

    fn departing_focus_decision_for(
        &mut self,
        hwnd: u64,
        was_tracked_focus: bool,
        cause: DepartureCause,
    ) -> crate::ui_sync::DepartingFocusDecision {
        let Some((foreground, foreground_is_valid)) = self.departing_foreground_evidence() else {
            return crate::ui_sync::DepartingFocusDecision {
                recover: false,
                suppress_landing_resync: false,
                replacement_hwnd: None,
            };
        };
        // Foreground evidence naming this HWND is the live replacement, not the
        // window that just left. Do not recover onto another managed window,
        // and do not bind the handle as a last-window replacement.
        if cause == DepartureCause::ReplacedLifetime && foreground == Some(hwnd) {
            return crate::ui_sync::DepartingFocusDecision {
                recover: false,
                suppress_landing_resync: true,
                replacement_hwnd: None,
            };
        }
        crate::ui_sync::departing_focus_decision(
            was_tracked_focus,
            hwnd,
            foreground,
            foreground_is_valid,
        )
    }

    pub(crate) fn departing_foreground_evidence(&mut self) -> Option<(Option<u64>, bool)> {
        #[cfg(test)]
        {
            self.departing_foreground_evidence_reads =
                self.departing_foreground_evidence_reads.saturating_add(1);
            let foreground = self.injected_foreground_hwnd;
            if let Some(next) = self.injected_next_foreground_hwnd.take() {
                self.injected_foreground_hwnd = Some(next);
            }
            foreground.map(|foreground| {
                (
                    foreground.filter(|&id| id != 0),
                    self.injected_foreground_is_valid
                        .unwrap_or(foreground.is_some_and(|id| id != 0)),
                )
            })
        }
        #[cfg(not(test))]
        {
            let foreground = leopardwm_platform_win32::get_foreground_window();
            let foreground_is_valid =
                foreground.is_some_and(leopardwm_platform_win32::is_valid_window);
            Some((foreground, foreground_is_valid))
        }
    }

    /// An Edit Config launch can raise a single-instance editor on another workspace.
    /// Pull that editor here instead of following focus; unrelated focus leaves
    /// the intent armed until its TTL expires.
    pub(crate) fn try_edit_config_pull(
        &mut self,
        hwnd: u64,
        monitor_id: leopardwm_platform_win32::MonitorId,
        ws_idx: usize,
    ) -> bool {
        let Some((set_at, config_name)) = self.pending_edit_config_pull.clone() else {
            return false;
        };
        if set_at.elapsed() >= EDIT_CONFIG_PULL_TTL {
            self.pending_edit_config_pull = None;
            return false;
        }
        // Identify the editor window: a single-instance editor shows the config
        // filename in its title. Any other cross-workspace focus is left alone,
        // since the editor may still raise within the TTL.
        let is_editor = self
            .lookup_window_info(hwnd)
            .is_some_and(|i| title_names_config_file(&i.title, &config_name));
        if !is_editor {
            return false;
        }
        // The editor was found: consume the arming so a later focus can't fire,
        // even if the pull below ends up being a no-op (already here / floating).
        self.pending_edit_config_pull = None;
        let target_mid = self.focused_monitor;
        let target_widx = self.active_workspace_idx(target_mid);
        if monitor_id == target_mid && ws_idx == target_widx {
            return false;
        }
        self.pull_window_to_workspace(hwnd, monitor_id, ws_idx, target_mid, target_widx)
    }

    pub(crate) fn pull_window_to_workspace(
        &mut self,
        hwnd: u64,
        from_mid: leopardwm_platform_win32::MonitorId,
        from_widx: usize,
        to_mid: leopardwm_platform_win32::MonitorId,
        to_widx: usize,
    ) -> bool {
        let is_tiled = self
            .workspaces
            .get(&from_mid)
            .and_then(|v| v.get(from_widx))
            .is_some_and(|ws| ws.contains_window(hwnd) && !ws.is_floating(hwnd));
        if !is_tiled {
            return false;
        }
        let tiled_width = self.tiled_column_width(from_mid, from_widx, hwnd);
        let snapshot = self.snapshot_layout();
        self.ensure_workspace_exists(to_mid, to_widx);
        let removed = self
            .workspaces
            .get_mut(&from_mid)
            .and_then(|v| v.get_mut(from_widx))
            .is_some_and(|ws| ws.remove_window(hwnd).is_ok());
        if !removed {
            return false;
        }
        // Transactional move: if the destination insert fails, put the window
        // back in its source workspace so it is never orphaned from layout state.
        let inserted = self
            .workspaces
            .get_mut(&to_mid)
            .and_then(|v| v.get_mut(to_widx))
            .is_some_and(|ws| ws.insert_window(hwnd, tiled_width).is_ok());
        if !inserted {
            if let Some(ws) = self
                .workspaces
                .get_mut(&from_mid)
                .and_then(|v| v.get_mut(from_widx))
            {
                let _ = ws.insert_window(hwnd, tiled_width);
            }
            return false;
        }
        if let Some(ws) = self
            .workspaces
            .get_mut(&to_mid)
            .and_then(|v| v.get_mut(to_widx))
        {
            let _ = ws.focus_window(hwnd);
        }
        // The pull establishes a fresh placement; void any prior move-back origin
        // so a later MoveToWorkspace can't restore it to a stale column.
        self.move_origins.remove(&hwnd);
        self.focused_monitor = to_mid;
        self.previous_focused_hwnd = Some(hwnd);
        if let Err(e) = leopardwm_platform_win32::set_foreground_window(hwnd) {
            debug!("Could not foreground pulled window {}: {:?}", hwnd, e);
        }
        self.start_layout_transition(snapshot);
        if let Err(e) = self.apply_layout() {
            warn!("Failed to apply layout after Edit Config pull: {}", e);
        }
        self.show_border(hwnd);
        self.broadcast_focused_window_if_changed(to_mid as i64, Some(hwnd));
        info!(
            "Pulled Edit Config editor window {} to the active workspace",
            hwnd
        );
        true
    }

    fn follow_workspace_without_stealing_focus(
        &mut self,
        monitor_id: leopardwm_platform_win32::MonitorId,
        ws_idx: usize,
        restore_focus_target: Option<u64>,
    ) {
        self.focused_monitor = monitor_id;

        // Auto-switch workspace if the focused window is on an inactive workspace
        // (e.g., user Alt+Tabbed to it)
        let active_idx = self.active_workspace_idx(monitor_id);
        if ws_idx == active_idx {
            return;
        }
        info!(
            "Auto-switching to workspace {} on monitor {} (focus follows window)",
            ws_idx + 1,
            monitor_id
        );

        // A live preview may have removed the dragged window from its source.
        // Restore membership only if the window survived, then remove placeholders.
        if let Some(drag) = self.drag_state.take() {
            if drag.removed_from_source
                && drag.is_tiled
                && leopardwm_platform_win32::is_valid_window(drag.hwnd)
            {
                if let Some(ws) = self
                    .workspaces
                    .get_mut(&drag.source_monitor)
                    .and_then(|v| v.get_mut(drag.source_workspace_idx))
                {
                    let _ = ws.insert_window(drag.hwnd, None);
                }
            }
            for ws_vec in self.workspaces.values_mut() {
                for ws in ws_vec.iter_mut() {
                    let _ = ws.remove_window(crate::state::DRAG_PLACEHOLDER_HWND);
                }
            }
        }
        self.pending_drag_hint = Some(crate::state::DragHintAction::Hide);
        if let Some(ref transition) = self.layout_transition {
            for wid in transition.exit_rects.keys() {
                if !self.is_application_fullscreen(*wid) {
                    #[cfg(not(test))]
                    if !self.paused {
                        let _ = leopardwm_platform_win32::move_window_offscreen(*wid);
                    }
                }
            }
        }
        // See the explicit-switch path: a replacement slide must carry the
        // deferred border instead of painting it on the way out.
        self.abort_active_ghost_transition();

        let slide_height = self
            .monitors
            .get(&monitor_id)
            .map(|m| m.work_area.height)
            .unwrap_or(crate::state::FALLBACK_WORK_AREA_HEIGHT);
        let y_offset = if ws_idx > active_idx {
            slide_height
        } else {
            -slide_height
        };

        let mut old_placements = self.workspace_placements(monitor_id, active_idx);
        old_placements.retain(|p| !self.is_application_fullscreen(p.window_id));

        self.active_workspace.insert(monitor_id, ws_idx);
        let viewport_width = self.viewport_width_for(monitor_id);
        if let Some(workspace) = self
            .workspaces
            .get_mut(&monitor_id)
            .and_then(|v| v.get_mut(ws_idx))
        {
            workspace.commit_pending_min_size_clears();
            workspace.reconcile_scroll_bounds(viewport_width);
        }

        let mut new_placements = self.workspace_placements(monitor_id, ws_idx);
        new_placements.retain(|p| !self.is_application_fullscreen(p.window_id));

        let mut start_rects = std::collections::HashMap::new();
        let mut exit_rects = std::collections::HashMap::new();
        self.prune_recently_restored_managed_windows();
        let restore_focus_target = restore_focus_target.filter(|hwnd| {
            self.recently_restored_managed_windows.contains_key(hwnd)
                && new_placements.iter().any(|p| p.window_id == *hwnd)
        });

        for placement in &new_placements {
            let rect = placement.rect;
            let start_y = if Some(placement.window_id) == restore_focus_target {
                rect.y
            } else {
                rect.y + y_offset
            };
            start_rects.insert(
                placement.window_id,
                leopardwm_core_layout::Rect::new(rect.x, start_y, rect.width, rect.height),
            );
        }
        if let Some(hwnd) = restore_focus_target {
            self.recently_restored_managed_windows.remove(&hwnd);
        }
        for placement in &old_placements {
            if placement.visibility != leopardwm_core_layout::Visibility::Visible {
                continue;
            }
            let rect = placement.rect;
            start_rects.insert(placement.window_id, rect);
            exit_rects.insert(
                placement.window_id,
                leopardwm_core_layout::Rect::new(
                    rect.x,
                    rect.y - y_offset,
                    rect.width,
                    rect.height,
                ),
            );
        }

        // As in handle_switch_workspace: animate only when motion isn't
        // reduced, otherwise hide the leaving windows immediately so they
        // don't linger as ghosts (reduce_motion skips the transition that
        // would otherwise move them off-screen).
        let animating = !start_rects.is_empty() && !self.reduce_motion;
        for placement in &old_placements {
            if !animating || placement.visibility != leopardwm_core_layout::Visibility::Visible {
                #[cfg(not(test))]
                if !self.paused {
                    let _ = leopardwm_platform_win32::move_window_offscreen(placement.window_id);
                }
            }
        }
        if animating {
            let duration = self.config.animation.workspace_switch_duration_ms;
            self.start_workspace_switch_transition(start_rects, exit_rects, duration);
        } else {
            self.abort_layout_transition();
        }
    }

    fn adopt_managed_replacement_without_stealing_focus(&mut self, hwnd: u64) -> bool {
        let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) else {
            return false;
        };
        self.follow_workspace_without_stealing_focus(monitor_id, ws_idx, None);
        if let Some(ref mut transition) = self.layout_transition {
            transition.suppress_landing_focus_resync = true;
        }
        let viewport_width = self.viewport_width_for(monitor_id);
        if let Some(workspace) = self
            .workspaces
            .get_mut(&monitor_id)
            .and_then(|v| v.get_mut(ws_idx))
        {
            if let Err(e) = workspace.focus_window(hwnd) {
                debug!("Failed to focus replacement window {}: {}", hwnd, e);
            } else {
                workspace.ensure_focused_visible_animated(viewport_width);
            }
        }
        if let Err(e) = self.apply_layout() {
            warn!("Failed to apply layout after replacement adoption: {}", e);
        }
        self.previous_focused_hwnd = Some(hwnd);
        self.last_focus_change_at = Some(std::time::Instant::now());
        self.show_border(hwnd);
        self.broadcast_focused_window_if_changed(monitor_id as i64, Some(hwnd));
        true
    }

    fn should_suppress_workspace_switch_focus(&mut self, hwnd: u64, event_time_ms: u32) -> bool {
        let Some(intent) = self.pending_workspace_switch_focus else {
            return false;
        };
        if !intent.is_fresh()
            || self.active_workspace_idx(intent.monitor) != intent.destination_workspace
        {
            self.pending_workspace_switch_focus = None;
            return false;
        }
        if hwnd != intent.source_hwnd {
            if self.find_window_workspace(hwnd).is_some()
                && !event_time_is_no_later_than(event_time_ms, intent.armed_at_event_time_ms)
            {
                self.pending_workspace_switch_focus = None;
            }
            return false;
        }
        if self.find_window_workspace(hwnd) != Some((intent.monitor, intent.source_workspace)) {
            self.pending_workspace_switch_focus = None;
            return false;
        }
        if event_time_is_no_later_than(event_time_ms, intent.armed_at_event_time_ms) {
            return true;
        }
        self.pending_workspace_switch_focus = None;
        false
    }

    pub(crate) fn selected_workspace_is_genuinely_empty(&self) -> bool {
        self.workspaces
            .get(&self.focused_monitor)
            .and_then(|workspaces| workspaces.get(self.active_workspace_idx(self.focused_monitor)))
            .is_none_or(workspace_is_genuinely_empty)
    }

    pub(crate) fn release_parked_foreground_for_empty_selection(&mut self) {
        if self.selected_workspace_is_genuinely_empty() {
            self.release_parked_foreground();
        }
    }

    /// Also release live hidden unmanaged foregrounds and transient console hosts,
    /// so their exit cannot return focus to a parked window and switch back.
    pub(crate) fn release_parked_foreground(&mut self) {
        #[cfg(test)]
        let foreground = self.injected_foreground_hwnd.flatten();
        #[cfg(not(test))]
        let foreground = leopardwm_platform_win32::get_foreground_window();
        let Some(hwnd) = foreground else {
            debug!("Skipping parked-foreground release because there is no OS foreground window");
            return;
        };
        let reason = if let Some((monitor, workspace_idx)) = self.find_window_workspace(hwnd) {
            if self.active_workspace_idx(monitor) == workspace_idx
                || self
                    .workspaces
                    .get(&monitor)
                    .and_then(|workspaces| workspaces.get(workspace_idx))
                    .is_some_and(|workspace| workspace.is_floating(hwnd))
            {
                return;
            }
            "parked managed"
        } else {
            if !leopardwm_platform_win32::is_valid_window(hwnd) {
                return;
            }
            if !self.is_window_visible_now(hwnd) {
                "hidden unmanaged"
            } else if self.lookup_window_info(hwnd).is_some_and(|info| {
                let executable = get_process_executable(info.process_id).unwrap_or_default();
                is_transient_console_host(&info.title, &info.class_name, &executable)
            }) {
                "transient console host"
            } else {
                return;
            }
        };

        info!(
            "Attempting foreground handoff for window {} to the focus placeholder: {}",
            hwnd, reason
        );
        let work_area = self
            .monitors
            .get(&self.focused_monitor)
            .map(|monitor| monitor.work_area);
        #[cfg(test)]
        if let Some(work_area) = work_area {
            self.foreground_release_requests.push((hwnd, work_area));
        }
        #[cfg(not(test))]
        if let (Some(placeholder), Some(work_area)) = (self.focus_placeholder.as_ref(), work_area) {
            match placeholder.release_foreground(hwnd, work_area) {
                Ok(true) => info!(
                    "Released foreground window {} to the focus placeholder",
                    hwnd
                ),
                Ok(false) => warn!(
                    "Windows refused foreground handoff for window {} to the focus placeholder",
                    hwnd
                ),
                Err(error) => warn!(
                    "Could not release foreground window {} to the focus placeholder: {:?}",
                    hwnd, error
                ),
            }
        }
    }

    /// Reconcile tab strips directly: an active transition defers apply_layout.
    pub(crate) fn clear_logical_focus_for_empty_selection(&mut self) {
        self.release_parked_foreground_for_empty_selection();
        self.previous_focused_hwnd = None;
        self.hide_border();
        self.update_tab_strip();
        let monitor = self.focused_monitor as i64;
        self.broadcast_focused_window_if_changed(monitor, None);
    }

    /// One foreground sample after a replaced-lifetime departure actually ran.
    ///
    /// Adopt only when that foreground is `hwnd` and it is now on the focused
    /// monitor's active workspace. Otherwise leave tracked focus as the
    /// departure left it. A foreground match also suppresses landing resync on
    /// the current transition, so a Created admission cannot land focus on
    /// another window.
    pub(crate) fn reconcile_replaced_lifetime_admission(&mut self, hwnd: u64) {
        let foreground_is_hwnd = self
            .departing_foreground_evidence()
            .is_some_and(|(foreground, _)| foreground == Some(hwnd));
        if foreground_is_hwnd {
            if let Some(transition) = self.layout_transition.as_mut() {
                transition.suppress_landing_focus_resync = true;
            }
            let on_selected_workspace = self.find_window_workspace(hwnd)
                == Some((
                    self.focused_monitor,
                    self.active_workspace_idx(self.focused_monitor),
                ));
            if on_selected_workspace && self.adopt_managed_replacement_without_stealing_focus(hwnd)
            {
                return;
            }
        }
        self.reconcile_border_without_stealing_focus();
    }

    pub(crate) fn arm_pending_last_window_departure(
        &mut self,
        replacement_hwnd: Option<u64>,
        armed_at_event_time_ms: u32,
        origin: LastWindowDepartureOrigin,
    ) {
        self.pending_last_window_departure = Some(PendingLastWindowDeparture {
            monitor: self.focused_monitor,
            workspace: self.active_workspace_idx(self.focused_monitor),
            replacement_hwnd,
            set_at: std::time::Instant::now(),
            armed_at_event_time_ms,
            origin,
        });
    }

    fn should_suppress_last_window_departure_focus(
        &mut self,
        hwnd: u64,
        event_time_ms: u32,
        monitor_id: leopardwm_platform_win32::MonitorId,
        ws_idx: usize,
    ) -> bool {
        let Some(intent) = self.pending_last_window_departure else {
            return false;
        };
        let handler_time_ms = self.event_time_now_ms();
        let ms_since_last_input = leopardwm_platform_win32::ms_since_last_user_input();
        let target_is_minimized = self
            .workspaces
            .get(&monitor_id)
            .and_then(|workspaces| workspaces.get(ws_idx))
            .is_some_and(|workspace| workspace.is_minimized(hwnd));
        // Keep Windows' departure handoff from undoing selection: suppress exact replacements and
        // bind eligible other-workspace handoffs; Minimized allows a 500 ms OS timestamp window.
        let event_is_eligible = match intent.origin {
            LastWindowDepartureOrigin::Minimized => event_time_is_no_later_than(
                event_time_ms,
                intent
                    .armed_at_event_time_ms
                    .wrapping_add(MINIMIZE_HANDOFF_WINDOW_MS),
            ),
            LastWindowDepartureOrigin::DirectDestroyedOrHidden
            | LastWindowDepartureOrigin::EventlessPrune => {
                event_time_is_no_later_than(event_time_ms, intent.armed_at_event_time_ms)
            }
        };
        let outcome = if !intent.is_fresh() {
            self.pending_last_window_departure = None;
            "stale_ttl"
        } else if self.focused_monitor != intent.monitor
            || self.active_workspace_idx(intent.monitor) != intent.workspace
        {
            self.pending_last_window_departure = None;
            "monitor_or_workspace_changed"
        } else if !event_is_eligible {
            self.pending_last_window_departure = None;
            if intent.origin == LastWindowDepartureOrigin::Minimized {
                "later_than_handoff_window"
            } else {
                "later_than_arm"
            }
        } else if intent.origin == LastWindowDepartureOrigin::Minimized && target_is_minimized {
            if intent.replacement_hwnd.is_none() || intent.replacement_hwnd == Some(hwnd) {
                self.pending_last_window_departure = None;
            }
            "target_still_minimized"
        } else if Some(hwnd) == intent.replacement_hwnd {
            "exact_replacement"
        } else if intent.replacement_hwnd.is_none()
            && matches!(
                intent.origin,
                LastWindowDepartureOrigin::DirectDestroyedOrHidden
                    | LastWindowDepartureOrigin::Minimized
            )
            && monitor_id == intent.monitor
            && ws_idx != intent.workspace
        {
            let mut pending = intent;
            pending.replacement_hwnd = Some(hwnd);
            self.pending_last_window_departure = Some(pending);
            "bound_inferred"
        } else {
            self.pending_last_window_departure = None;
            "not_inferable"
        };
        let suppressed = matches!(outcome, "exact_replacement" | "bound_inferred");
        debug!(
            target: "leopardwm::event_handler",
            hwnd,
            event_time_ms,
            handler_time_ms,
            ms_since_last_input = ?ms_since_last_input,
            intent_origin = ?intent.origin,
            intent_monitor = ?intent.monitor,
            intent_workspace = intent.workspace,
            intent_replacement_hwnd = ?intent.replacement_hwnd,
            intent_armed_at_event_time_ms = intent.armed_at_event_time_ms,
            event_monitor = ?monitor_id,
            event_workspace = ws_idx,
            outcome,
            suppressed,
            "last-window departure focus guard consulted"
        );
        suppressed
    }

    fn prune_recently_restored_managed_windows(&mut self) {
        self.recently_restored_managed_windows
            .retain(|_, restored_at| restored_at.elapsed() < RECENTLY_RESTORED_MANAGED_WINDOW_TTL);
    }

    fn is_recently_restored_managed_window(&mut self, hwnd: u64) -> bool {
        self.prune_recently_restored_managed_windows();
        self.recently_restored_managed_windows.contains_key(&hwnd)
    }

    fn floating_raise_workspace(
        &self,
        hwnd: u64,
    ) -> Option<(leopardwm_platform_win32::MonitorId, &Workspace)> {
        if !self.config.behavior.floating_above_tiled
            || !self.config.behavior.track_focus_changes
            || self.overview_open
            || self.paused
        {
            return None;
        }
        let (monitor, index) = self.find_window_workspace(hwnd)?;
        if index != self.active_workspace_idx(monitor) {
            return None;
        }
        let workspaces = &self.workspaces[&monitor];
        let workspace = &workspaces[index];
        if workspace.is_floating(hwnd)
            || workspace.is_minimized(hwnd)
            || workspaces.iter().any(Workspace::is_fullscreen)
            || self
                .application_fullscreen
                .values()
                .any(|session| session.monitor_id == monitor)
        {
            return None;
        }
        Some((monitor, workspace))
    }

    pub(crate) fn floating_raise_order(
        &self,
        hwnd: u64,
        normal_z_order: &[u64],
        native_fullscreen: bool,
    ) -> Vec<u64> {
        let Some((_, workspace)) = self.floating_raise_workspace(hwnd) else {
            return Vec::new();
        };
        if native_fullscreen {
            return Vec::new();
        }
        let Some(focused_position) = normal_z_order.iter().position(|&id| id == hwnd) else {
            return Vec::new();
        };
        let floating: Vec<_> = normal_z_order
            .iter()
            .enumerate()
            .filter(|(_, id)| workspace.is_floating(**id) && !workspace.is_minimized(**id))
            .collect();
        if floating
            .iter()
            .all(|(position, _)| *position < focused_position)
        {
            return Vec::new();
        }
        floating.into_iter().rev().map(|(_, &id)| id).collect()
    }

    fn raise_floating_above_tiled(&self, hwnd: u64) {
        #[cfg(not(test))]
        {
            if leopardwm_platform_win32::get_foreground_window() != Some(hwnd) {
                return;
            }
            if self.floating_raise_workspace(hwnd).is_none() {
                return;
            }
            let (chrome, dwm) = self.application_fullscreen_geometry(hwnd);
            let native_fullscreen = self
                .observe_application_fullscreen(
                    hwnd,
                    chrome,
                    dwm,
                    leopardwm_platform_win32::is_window_maximized(hwnd),
                )
                .is_some();
            if native_fullscreen {
                return;
            }
            let normal_z_order: Vec<_> = leopardwm_platform_win32::visible_window_z_order()
                .into_iter()
                .filter(|&id| leopardwm_platform_win32::can_raise_normal_window(id))
                .collect();
            for id in self.floating_raise_order(hwnd, &normal_z_order, native_fullscreen) {
                if leopardwm_platform_win32::get_foreground_window() != Some(hwnd) {
                    break;
                }
                if let Err(error) = leopardwm_platform_win32::raise_normal_window_no_activate(id) {
                    warn!(
                        "Could not raise floating window {} without activation: {}",
                        id, error
                    );
                }
            }
        }
        #[cfg(test)]
        let _ = hwnd;
    }

    fn on_window_focused(&mut self, hwnd: u64, event_time_ms: u32) {
        // Skip if this window is already our tracked focus — avoids
        // feedback loops where sync_foreground_window triggers another
        // EVENT_SYSTEM_FOREGROUND for the same window.
        if self.previous_focused_hwnd == Some(hwnd) {
            return;
        }

        // Suppress rapid same-column focus switches caused by mouse wheel
        // scrolling near the boundary between stacked windows. Windows'
        // "scroll inactive windows" feature can cause the foreground to
        // ping-pong between adjacent windows during rapid scrolling.
        //
        // Exception: a tab-strip click or `Ctrl+Alt+J/K` cycle in a
        // Tabbed column synthesizes a deliberate same-column focus
        // change. The command handler sets `pending_tab_focus`
        // before triggering it; we consume that flag here so the
        // expected event flows through.
        let now = std::time::Instant::now();
        if let Some(prev_hwnd) = self.previous_focused_hwnd {
            if let Some(last_change) = self.last_focus_change_at {
                if now.duration_since(last_change).as_millis() < 200 {
                    if let Some((mon_a, ws_a)) = self.find_window_workspace(prev_hwnd) {
                        if let Some((mon_b, ws_b)) = self.find_window_workspace(hwnd) {
                            if mon_a == mon_b && ws_a == ws_b {
                                let same_col = self.workspaces.get(&mon_a)
                                    .and_then(|v| v.get(ws_a))
                                    .is_some_and(|ws| {
                                        let loc_a = ws.find_window_location_rc(prev_hwnd);
                                        let loc_b = ws.find_window_location_rc(hwnd);
                                        matches!((loc_a, loc_b), (Some((ra, ca, _)), Some((rb, cb, _))) if ra == rb && ca == cb)
                                    });
                                if same_col {
                                    let consumed =
                                        self.consume_pending_tab_focus_for(mon_a, ws_a, hwnd);
                                    if !consumed {
                                        debug!(
                                            "Suppressed rapid same-column focus switch: {} -> {}",
                                            prev_hwnd, hwnd
                                        );
                                        return;
                                    }
                                    debug!(
                                        "Same-column focus switch allowed (tab intent): {} -> {}",
                                        prev_hwnd, hwnd
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        let focused_restore_activation = self.is_recently_restored_managed_window(hwnd)
            || self
                .find_window_workspace(hwnd)
                .and_then(|(monitor, workspace)| {
                    self.workspaces
                        .get(&monitor)
                        .and_then(|workspaces| workspaces.get(workspace))
                })
                .is_some_and(|workspace| workspace.is_minimized(hwnd));

        // Reconcile: prune windows that vanished without events
        // (e.g., Electron close-to-tray apps).
        // Throttle to at most once per second to avoid per-event overhead.
        if self
            .last_prune_at
            .is_none_or(|t| now.duration_since(t).as_secs() >= 1)
        {
            self.last_prune_at = Some(now);
            let pre_count = self.all_managed_window_ids().len();
            let tracked = self.previous_focused_hwnd;
            let prune = self.prune_stale_windows_with(Some(crate::helpers::FocusedPruneContext {
                focused_hwnd: hwnd,
                tracked,
                event_time_ms,
                restore_activation: focused_restore_activation,
            }));
            let pruned = pre_count - self.all_managed_window_ids().len();
            match prune {
                crate::helpers::StalePruneLayout::Applied => {}
                crate::helpers::StalePruneLayout::Failed(e) => {
                    warn!(
                        "Failed to apply layout after pruning {} stale window(s): {}",
                        pruned, e
                    );
                }
                crate::helpers::StalePruneLayout::Unchanged if pruned > 0 => {
                    if let Err(e) = self.apply_layout() {
                        warn!(
                            "Failed to apply layout after pruning {} stale window(s): {}",
                            pruned, e
                        );
                    }
                }
                crate::helpers::StalePruneLayout::Unchanged => {}
            }
        }

        if let Some(tracked) = self.previous_focused_hwnd {
            if let Some((monitor_id, ws_idx)) = self.find_window_workspace(tracked) {
                let marked_minimized = self
                    .workspaces
                    .get(&monitor_id)
                    .and_then(|workspaces| workspaces.get(ws_idx))
                    .is_some_and(|workspace| workspace.is_minimized(tracked));
                if !marked_minimized
                    && self.stale_window_probe(tracked).0
                        == leopardwm_platform_win32::WindowPresence::Minimized
                {
                    if (monitor_id, ws_idx)
                        == (
                            self.focused_monitor,
                            self.active_workspace_idx(self.focused_monitor),
                        )
                    {
                        if focused_restore_activation {
                            self.reconcile_minimized_without_departure(tracked);
                        } else {
                            self.on_window_minimized_with_context(
                                tracked,
                                None,
                                Some(event_time_ms),
                                "focused_reconcile",
                            );
                        }
                    } else {
                        self.reconcile_minimized_without_departure(tracked);
                    }
                }
            }
        }

        if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
            // A single-instance editor launched from the tray ("Edit Config")
            // may have raised an existing window on another workspace; pull it
            // to the active workspace instead of following focus there.
            if self.try_edit_config_pull(hwnd, monitor_id, ws_idx) {
                return;
            }
            if self.should_suppress_workspace_switch_focus(hwnd, event_time_ms) {
                return;
            }
            if self.suppress_background_rejoin_activation(hwnd, event_time_ms) {
                return;
            }
            let recently_restored_focus = self
                .is_recently_restored_managed_window(hwnd)
                .then_some(hwnd);
            if self.should_suppress_last_window_departure_focus(
                hwnd,
                event_time_ms,
                monitor_id,
                ws_idx,
            ) {
                return;
            }
            self.follow_workspace_without_stealing_focus(
                monitor_id,
                ws_idx,
                recently_restored_focus,
            );

            let viewport_width = self.viewport_width_for(monitor_id);

            // Distinguish user-initiated focus changes (clicks /
            // hotkeys / Alt-Tab) from spurious foreground events
            // fired by background apps. Without recent user input
            // we still update internal focus tracking but skip the
            // auto-scroll that would yank the viewport to a window
            // the user did not actually request — the classic
            // "I was on Terminal and Zen suddenly stole focus and
            // scrolled the layout" symptom.
            //
            // Threshold is generous (1.5 s) because a Focused event
            // delivered through WinEventProc -> our hook -> tokio
            // mpsc -> daemon mutex can lag well past 500 ms when the
            // daemon is busy or DWM is loaded. Spurious events from
            // notification toasts and tray apps fire from timers
            // unrelated to user input, so even at 1.5 s the false-
            // positive rate stays low. Fail CLOSED on
            // `GetLastInputInfo` failure — if the API ever returns
            // None we cannot prove user intent, so we don't auto-
            // scroll. The user can still hotkey the focus shift,
            // which goes through `command_handler` and bypasses
            // this gate entirely.
            const FOCUS_INPUT_RECENT_MS: u32 = 1500;
            let user_initiated = leopardwm_platform_win32::ms_since_last_user_input()
                .map(|ms| ms <= FOCUS_INPUT_RECENT_MS)
                .unwrap_or(false);
            // A non-user-initiated focus event for a window other than the
            // fullscreen one (e.g. a window that just opened behind a fullscreen
            // window and self-activated) must not pull focus off the fullscreen
            // window: following it desyncs the layout from what's on screen and
            // aims the next focus command at a hidden window.
            let fullscreen = self
                .workspaces
                .get(&monitor_id)
                .and_then(|v| v.get(ws_idx))
                .and_then(|ws| ws.fullscreen_window_id());
            if let Some(fs_wid) = fullscreen_focus_guard(user_initiated, fullscreen, hwnd) {
                debug!(
                    "Keeping fullscreen window {} focused; ignoring non-user focus to {}",
                    fs_wid, hwnd
                );
                self.reassert_fullscreen_focus(fs_wid);
                return;
            }
            if let Some(workspace) = self
                .workspaces
                .get_mut(&monitor_id)
                .and_then(|v| v.get_mut(ws_idx))
            {
                if let Err(e) = workspace.focus_window(hwnd) {
                    // Floating windows are not in the tiled column list,
                    // so focus_window fails for them — that's expected.
                    debug!("Failed to focus window {}: {}", hwnd, e);
                } else {
                    debug!(
                        "Focus changed to window {} on monitor {} (user_initiated={})",
                        hwnd, monitor_id, user_initiated
                    );
                    if user_initiated {
                        workspace.ensure_focused_visible_animated(viewport_width);
                    }
                }
            }
            // Always apply layout — even if focus_window failed (floating windows),
            // we still need to repaint if we just switched workspaces.
            if let Err(e) = self.apply_layout() {
                warn!("Failed to apply layout after focus change: {}", e);
            }

            // Update border only — do NOT call sync_foreground_window()
            // here because the window is already focused (that's why we
            // received this event). Calling set_foreground_window again
            // would trigger another EVENT_SYSTEM_FOREGROUND feedback loop.
            self.show_border(hwnd);

            // Track the OS-foreground window — including floating windows —
            // so that ToggleFloating can reliably detect and unfloat the
            // currently focused floating window.
            self.previous_focused_hwnd = Some(hwnd);
            self.last_focus_change_at = Some(now);
            self.broadcast_focused_window_if_changed(monitor_id as i64, Some(hwnd));
        } else {
            self.on_unmanaged_window_focused(hwnd);
        }
    }

    fn on_unmanaged_window_focused(&mut self, hwnd: u64) {
        if matches!(
            self.temporary_ignore_gate(hwnd),
            crate::temporary_ignore::IgnoreGate::Block
        ) {
            self.hide_border();
            self.previous_focused_hwnd = None;
            let monitor_id = self.focused_monitor as i64;
            self.broadcast_focused_window_if_changed(monitor_id, None);
            return;
        }

        // Recovery path: if a user focuses a window that was
        // suppressed by recently_hidden_hwnds (e.g., tray-restored
        // app), re-add it now. A user focusing a window proves it's
        // not a transient popup.
        //
        // A recycled handle drops the entry and is not recovered.
        // Otherwise peek first. Lookup failure or an ordinary Ignore keeps
        // suppression for later focus recovery. A size-conditioned Ignore
        // hands recovery to size/title retries, which need suppression removed.
        if self.same_lifetime_recently_hidden(hwnd) {
            if let Some(win_info) = self.lookup_window_info(hwnd) {
                let executable = get_process_executable(win_info.process_id).unwrap_or_default();
                let action = self
                    .admission_rule(&win_info, &executable)
                    .map(|rule| rule.action)
                    .unwrap_or(config::WindowAction::Tile);
                if self.size_ignored_windows.contains(&hwnd) {
                    self.recently_hidden_hwnds.remove(&hwnd);
                }
                if action != config::WindowAction::Ignore {
                    info!(
                        "Recovering suppressed window: {} ({}) - user focused it",
                        win_info.title, win_info.class_name
                    );
                    // Consume the entry now (immediately before
                    // dispatch) so the Created handler doesn't
                    // re-suppress on this same recovery path.
                    self.recently_hidden_hwnds.remove(&hwnd);
                    self.admit_created_without_event_time(hwnd);
                    // Update tiled focus to match OS — the user just
                    // focused this window. focus_window may fail for
                    // floating windows, which is fine.
                    let recovery_monitor =
                        if let Some((mid, widx)) = self.find_window_workspace(hwnd) {
                            if let Some(ws) =
                                self.workspaces.get_mut(&mid).and_then(|v| v.get_mut(widx))
                            {
                                let _ = ws.focus_window(hwnd);
                            }
                            mid
                        } else {
                            self.focused_monitor
                        };
                    self.previous_focused_hwnd = Some(hwnd);
                    self.show_border(hwnd);
                    self.broadcast_focused_window_if_changed(recovery_monitor as i64, Some(hwnd));
                    return;
                }
            }
        }

        // Recovery path for the transient-console-host filter:
        // a real interactive PowerShell or cmd window may have
        // hit the filter at Created time if its title was still
        // the exe path. By the time the user actually focuses
        // it the title has been set (e.g. "Administrator:
        // Windows PowerShell"), so re-check and re-add. A user
        // focusing the window proves it is not a transient
        // scheduled-task spawn.
        if let Some(win_info) = self.lookup_window_info(hwnd) {
            if win_info.class_name == "ConsoleWindowClass" {
                let executable = get_process_executable(win_info.process_id).unwrap_or_default();
                let title_lower = win_info.title.to_ascii_lowercase();
                let title_still_exe_path = title_lower.ends_with(".exe")
                    || (!executable.is_empty() && title_lower == executable.to_ascii_lowercase());
                if !title_still_exe_path {
                    let action = self
                        .admission_rule(&win_info, &executable)
                        .map(|rule| rule.action)
                        .unwrap_or(config::WindowAction::Tile);
                    if action != config::WindowAction::Ignore {
                        info!(
                            "Recovering console-host window with real title: {} ({}) - user focused it",
                            win_info.title, win_info.class_name
                        );
                        self.admit_created_without_event_time(hwnd);
                        let recovery_monitor =
                            if let Some((mid, widx)) = self.find_window_workspace(hwnd) {
                                if let Some(ws) =
                                    self.workspaces.get_mut(&mid).and_then(|v| v.get_mut(widx))
                                {
                                    let _ = ws.focus_window(hwnd);
                                }
                                mid
                            } else {
                                self.focused_monitor
                            };
                        self.previous_focused_hwnd = Some(hwnd);
                        self.show_border(hwnd);
                        self.broadcast_focused_window_if_changed(
                            recovery_monitor as i64,
                            Some(hwnd),
                        );
                        return;
                    }
                }
            }
        }

        // Focus went to an unmanaged window (e.g. settings, taskbar).
        // Hide the border overlay and clear tracked hwnd so animation
        // frames don't re-show it.
        self.hide_border();
        self.previous_focused_hwnd = None;
        let monitor_id = self.focused_monitor as i64;
        self.broadcast_focused_window_if_changed(monitor_id, None);
    }

    pub(crate) fn mark_minimized_and_reflow(&mut self, hwnd: u64) -> Option<bool> {
        let (monitor_id, ws_idx) = self.find_window_workspace(hwnd)?;
        let viewport_width = self.viewport_width_for(monitor_id);
        let layout_viewport = self.layout_viewport(monitor_id);
        let is_floating = self
            .workspaces
            .get(&monitor_id)
            .and_then(|workspaces| workspaces.get(ws_idx))
            .is_some_and(|workspace| workspace.is_floating(hwnd));
        let workspace = self
            .workspaces
            .get_mut(&monitor_id)
            .and_then(|workspaces| workspaces.get_mut(ws_idx))?;
        let cleared_fullscreen = workspace.clear_fullscreen_if_window(hwnd);
        // mark_minimized only handles tiled windows; floating windows are not
        // in the minimized set, so both paths count as a handled minimize.
        if !(workspace.mark_minimized(hwnd) || cleared_fullscreen || is_floating) {
            return None;
        }
        if is_floating && self.previous_focused_hwnd == Some(hwnd) {
            self.previous_focused_hwnd = None;
        }

        let col_info = workspace
            .find_window_location_rc(hwnd)
            .and_then(|(r, ci, _)| {
                workspace.columns_for_row(r).get(ci).map(|col| {
                    let visible = col
                        .windows()
                        .iter()
                        .filter(|w| !workspace.is_minimized(**w))
                        .count();
                    (ci, col.len(), visible)
                })
            });
        info!(
            "Window {} minimized (col={:?}, minimized_total={})",
            hwnd,
            col_info,
            workspace.minimized_count()
        );

        if workspace.focused_window() == Some(hwnd) {
            workspace.focus_down();
            if workspace.focused_window() == Some(hwnd) {
                workspace.focus_up();
            }
            if workspace.focused_window() == Some(hwnd) {
                workspace.focus_right();
                if workspace.focused_window() == Some(hwnd) {
                    workspace.focus_left();
                }
            }
        }
        workspace.ensure_focused_visible_animated(viewport_width);

        for placement in workspace.compute_placements(layout_viewport) {
            info!(
                "  post-minimize placement: hwnd={} rect=({},{} {}x{})",
                placement.window_id,
                placement.rect.x,
                placement.rect.y,
                placement.rect.width,
                placement.rect.height,
            );
        }
        Some(workspace.focused_visible_window().is_some())
    }

    pub(crate) fn reconcile_minimized_without_departure(&mut self, hwnd: u64) {
        let snapshot = self.snapshot_layout();
        if self.mark_minimized_and_reflow(hwnd).is_some() {
            if self.start_layout_transition(snapshot) {
                if let Some(transition) = self.layout_transition.as_mut() {
                    transition.suppress_landing_focus_resync = true;
                }
            }
            if let Err(e) = self.apply_layout() {
                warn!(
                    "Failed to apply layout after minimize reconciliation: {}",
                    e
                );
            }
        }
    }

    fn on_window_minimized_from_event(&mut self, hwnd: u64, os_event_time_ms: u32) {
        self.on_window_minimized_with_context(hwnd, None, Some(os_event_time_ms), "event");
    }

    pub(crate) fn on_window_minimized_with_snapshot(
        &mut self,
        hwnd: u64,
        layout_snapshot: Option<std::collections::HashMap<u64, leopardwm_core_layout::Rect>>,
    ) {
        self.on_window_minimized_with_snapshot_at(hwnd, layout_snapshot, None);
    }

    pub(crate) fn on_window_minimized_with_snapshot_at(
        &mut self,
        hwnd: u64,
        layout_snapshot: Option<std::collections::HashMap<u64, leopardwm_core_layout::Rect>>,
        event_time_ms: Option<u32>,
    ) {
        self.on_window_minimized_with_context(
            hwnd,
            layout_snapshot,
            event_time_ms,
            "prune_reconcile",
        );
    }

    fn on_window_minimized_with_context(
        &mut self,
        hwnd: u64,
        layout_snapshot: Option<std::collections::HashMap<u64, leopardwm_core_layout::Rect>>,
        os_event_time_ms: Option<u32>,
        source: &'static str,
    ) -> bool {
        let handler_time_ms = self.event_time_now_ms();
        let ms_since_last_input = leopardwm_platform_win32::ms_since_last_user_input();
        let was_tracked_focus = self.previous_focused_hwnd == Some(hwnd);
        let mut was_selected_focus = false;
        let mut has_focused_visible_window = false;
        let mut armed = false;
        let mut decision_reason = "not_managed";
        if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
            was_selected_focus = was_tracked_focus
                && self.focused_monitor == monitor_id
                && self.active_workspace_idx(monitor_id) == ws_idx;
            let snapshot = layout_snapshot.unwrap_or_else(|| self.snapshot_layout());
            if let Some(has_visible_window) = self.mark_minimized_and_reflow(hwnd) {
                has_focused_visible_window = has_visible_window;
                self.start_layout_transition(snapshot);
                if let Err(e) = self.apply_layout() {
                    warn!("Failed to apply layout after minimize: {}", e);
                }
                self.focused_monitor = monitor_id;
                if was_selected_focus && has_focused_visible_window {
                    let armed_at_event_time_ms =
                        os_event_time_ms.unwrap_or_else(|| self.event_time_now_ms());
                    self.arm_pending_last_window_departure(
                        None,
                        armed_at_event_time_ms,
                        LastWindowDepartureOrigin::Minimized,
                    );
                    armed = true;
                    decision_reason = "armed";
                } else if !was_tracked_focus {
                    decision_reason = "not_tracked_focus";
                } else if !was_selected_focus {
                    decision_reason = "not_selected_focus";
                } else {
                    decision_reason = "no_focused_visible_window";
                }
                self.sync_foreground_window();
            } else {
                decision_reason = "minimize_not_applied";
            }
        }
        debug!(
            target: "leopardwm::event_handler",
            hwnd,
            source,
            os_event_time_ms = ?os_event_time_ms,
            handler_time_ms,
            ms_since_last_input = ?ms_since_last_input,
            was_tracked_focus,
            was_selected_focus,
            has_focused_visible_window,
            armed,
            reason = decision_reason,
            "minimize departure guard decision"
        );
        armed
    }

    fn on_window_restored(&mut self, hwnd: u64) {
        if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
            let viewport_width = self.viewport_width_for(monitor_id);
            let is_active_workspace = self.active_workspace_idx(monitor_id) == ws_idx;
            let snapshot = self.snapshot_layout();
            let mut should_sync_foreground = false;
            let mut was_tiled_restore = false;
            let mut did_restore = false;
            if let Some(workspace) = self
                .workspaces
                .get_mut(&monitor_id)
                .and_then(|v| v.get_mut(ws_idx))
            {
                if workspace.mark_restored(hwnd) {
                    did_restore = true;
                    info!("Window {} restored from minimized", hwnd);
                    if workspace.is_floating(hwnd) {
                        // Keep floating restores from stealing focus back to tiled windows.
                        debug!(
                            "Restored floating window {} without changing tiled focus",
                            hwnd
                        );
                    } else if let Err(e) = workspace.focus_window(hwnd) {
                        warn!("Failed to focus restored window {}: {}", hwnd, e);
                    } else {
                        workspace.ensure_focused_visible_animated(viewport_width);
                        should_sync_foreground = is_active_workspace;
                        was_tiled_restore = true;
                    }
                }
            }
            if did_restore {
                let restored_at = std::time::Instant::now();
                self.prune_recently_restored_managed_windows();
                self.recently_restored_managed_windows
                    .insert(hwnd, restored_at);
            }
            if did_restore
                && self.pending_last_window_departure.is_some_and(|intent| {
                    intent.monitor == monitor_id
                        && intent.origin == LastWindowDepartureOrigin::Minimized
                        && (intent.replacement_hwnd.is_none()
                            || intent.replacement_hwnd == Some(hwnd))
                })
            {
                let intent = self.pending_last_window_departure.unwrap();
                self.pending_last_window_departure = None;
                debug!(
                    target: "leopardwm::event_handler",
                    hwnd,
                    intent_origin = ?intent.origin,
                    intent_monitor = ?intent.monitor,
                    intent_workspace = intent.workspace,
                    intent_replacement_hwnd = ?intent.replacement_hwnd,
                    intent_armed_at_event_time_ms = intent.armed_at_event_time_ms,
                    "restored window cleared minimized departure guard"
                );
            }
            if was_tiled_restore {
                self.start_layout_transition(snapshot);
            }
            if let Err(e) = self.apply_layout() {
                warn!("Failed to apply layout after window restore: {}", e);
            }
            if should_sync_foreground {
                self.focused_monitor = monitor_id;
                self.sync_foreground_window();
            }
        } else {
            // The daemon's startup enumeration skips IsIconic windows, so
            // tray apps that boot in a minimized state (Raw Accel, Discord
            // close-to-tray, Spotify minimized) never enter the managed set
            // until they are restored. Treating an unmanaged restore as a
            // Created event lets the standard rule/tile pipeline pick them
            // up the first time the user actually brings them on screen.
            debug!(
                "Window {} restored (unmanaged) — re-dispatching as Created",
                hwnd
            );
            self.admit_created_without_event_time(hwnd);
        }
    }

    pub(crate) fn cancel_matching_unfinished_move_size_ui(
        &mut self,
        hwnd: u64,
    ) -> MoveSizeCancelResult {
        let cancelled_resize = self.cancel_matching_unfinished_resize(hwnd);
        let (cancelled_drag, removed_placeholder, removed_from_source, drag_source) =
            self.cancel_matching_unfinished_drag(hwnd);
        if cancelled_resize || cancelled_drag {
            self.hide_move_size_hint_if_unowned(hwnd);
        }
        MoveSizeCancelResult {
            cancelled_resize,
            cancelled_drag,
            removed_placeholder,
            removed_from_source,
            drag_source,
        }
    }

    fn hide_move_size_hint_if_unowned(&mut self, hwnd: u64) {
        let peer_drag = self.drag_state.as_ref().is_some_and(|d| d.hwnd != hwnd);
        let peer_resize = self.resize_hwnd.is_some_and(|id| id != hwnd);
        if !peer_drag && !peer_resize {
            self.pending_drag_hint = Some(crate::state::DragHintAction::Hide);
        }
    }

    fn cancel_matching_unfinished_resize(&mut self, hwnd: u64) -> bool {
        if self.resize_hwnd != Some(hwnd) {
            return false;
        }
        self.resize_hwnd = None;
        self.clear_resize_preview_state();
        true
    }

    fn teardown_resize_preview_ui(&mut self) {
        self.pending_drag_hint = Some(crate::state::DragHintAction::Hide);
        self.clear_resize_preview_state();
    }

    fn clear_resize_preview_state(&mut self) {
        self.resize_preview_cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.resize_preview_target = None;
        self.resize_preview_display_rect = None;
        self.pending_resize_animation = None;
        self.last_resize_hint_update = None;
    }

    fn cancel_matching_unfinished_drag(
        &mut self,
        hwnd: u64,
    ) -> (
        bool,
        bool,
        bool,
        Option<(leopardwm_platform_win32::MonitorId, usize)>,
    ) {
        let Some(drag) = self.drag_state.as_ref() else {
            return (false, false, false, None);
        };
        if drag.hwnd != hwnd {
            return (false, false, false, None);
        }
        let drag_hwnd = drag.hwnd;
        let removed_from_source = drag.removed_from_source;
        let drag_source = Some((drag.source_monitor, drag.source_workspace_idx));
        self.drag_state = None;
        let removed_placeholder = self.clear_drag_placeholder();
        if leopardwm_platform_win32::is_valid_window(drag_hwnd) {
            leopardwm_platform_win32::set_dwm_transitions_disabled(drag_hwnd, false);
        }
        (true, removed_placeholder, removed_from_source, drag_source)
    }

    fn on_move_size_start(&mut self, hwnd: u64) {
        debug!("User started dragging/resizing window {}", hwnd);

        // Distinguish resize (border drag) from move (title bar drag).
        // Only create drag state for moves — resizes should not trigger
        // the drag-and-drop overlay.
        if leopardwm_platform_win32::is_cursor_on_resize_border(hwnd) {
            debug!("Detected resize (not move) for window {}, tracking", hwnd);
            self.resize_hwnd = Some(hwnd);
            return;
        }

        let (
            is_tiled,
            source_monitor,
            source_ws_idx,
            source_row,
            source_window_slot,
            col_idx,
            source_column_width,
        ) = if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
            let is_floating = self
                .workspaces
                .get(&monitor_id)
                .and_then(|v| v.get(ws_idx))
                .is_none_or(|ws| ws.is_floating(hwnd));
            let (source_row, source_window_slot, col_idx, source_column_width) = if !is_floating {
                self.workspaces
                    .get(&monitor_id)
                    .and_then(|v| v.get(ws_idx))
                    .and_then(|ws| {
                        ws.find_window_location_rc(hwnd).map(|(row, col, slot)| {
                            let width = ws
                                .rows()
                                .get(row)
                                .and_then(|r| r.column(col))
                                .map(|c| c.width());
                            (row, slot, col, width)
                        })
                    })
                    .unwrap_or((0, 0, 0, None))
            } else {
                (0, 0, 0, None)
            };
            (
                !is_floating,
                monitor_id,
                ws_idx,
                source_row,
                source_window_slot,
                col_idx,
                source_column_width,
            )
        } else {
            (
                false,
                self.focused_monitor,
                self.active_workspace_idx(self.focused_monitor),
                0,
                0,
                0,
                None,
            )
        };
        self.drag_state = Some(DragState {
            hwnd,
            is_tiled,
            source_monitor,
            source_workspace_idx: source_ws_idx,
            source_row,
            source_window_slot,
            current_row_index: source_row,
            current_column_index: col_idx,
            last_drop_target: None,
            last_hint_update: None,
            removed_from_source: false,
            preview_mode: crate::state::DragPreviewMode::None,
            target_column_peers: Vec::new(),
            source_column_peers: Vec::new(),
            source_column_width,
        });
        // Disable DWM-managed position interpolation on the
        // dragged window so its final SetWindowPos on drop
        // lands instantly. Without this, DWM smooths the
        // transition between the drop point and the layout
        // slot — the user perceives this as the column
        // "sliding" into place when dropping into a Tabbed
        // target where no layout-transition animation is
        // running.
        leopardwm_platform_win32::set_dwm_transitions_disabled(hwnd, true);
    }

    fn on_move_size_end(&mut self, hwnd: u64) {
        debug!("User finished dragging/resizing window {}", hwnd);

        // The dragged/resized window has physically drifted from its
        // layout slot. Evict its last_placed entry so apply_layout's
        // fast-path can't short-circuit on no-layout-change drop
        // paths (small in-column drag → snap_back_tiled, single-
        // window same-column merge, resize that lands within the
        // existing preset bucket). Without this the window is left
        // wherever the user released it until something else
        // triggers a real layout change.
        self.last_placed_layout_rects.remove(&hwnd);

        if self.resize_hwnd == Some(hwnd) {
            self.resize_hwnd = None;
            self.handle_resize_complete(hwnd);
            // Restore DWM transitions on this early-return path so they cannot
            // stay suppressed for the rest of the window's lifetime.
            leopardwm_platform_win32::set_dwm_transitions_disabled(hwnd, false);
            return;
        }

        // Verify this MoveSizeEnd matches the active drag — a mismatched
        // event for a different window should not tear down the drag state.
        if self.drag_state.as_ref().is_some_and(|d| d.hwnd != hwnd) {
            debug!(
                "Ignoring MoveSizeEnd for {} — drag active for different window",
                hwnd
            );
            // Mismatched event — re-enable transitions on the hwnd
            // we just got the event for; the original drag's hwnd
            // will re-enable on its own MoveSizeEnd.
            leopardwm_platform_win32::set_dwm_transitions_disabled(hwnd, false);
            return;
        }

        let drag = self.drag_state.take();
        self.pending_drag_hint = Some(DragHintAction::Hide);

        let Some(drag) = drag else {
            leopardwm_platform_win32::set_dwm_transitions_disabled(hwnd, false);
            return;
        };

        if !drag.is_tiled {
            // Floating geometry uses the visible DWM rect, not GetWindowRect.
            // Placement adds invisible-border insets; storing the outer rect would
            // grow the window by about 14 px on each drop.
            if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
                if let Some(visible_rect) = leopardwm_platform_win32::get_window_visible_rect(hwnd)
                {
                    if let Some(workspace) = self
                        .workspaces
                        .get_mut(&monitor_id)
                        .and_then(|v| v.get_mut(ws_idx))
                    {
                        workspace.update_floating(hwnd, visible_rect);
                        debug!("Floating window {} dropped at {:?}", hwnd, visible_rect);
                    }
                }
            }
            leopardwm_platform_win32::set_dwm_transitions_disabled(hwnd, false);
            return;
        }

        let Some(win_info) = self.lookup_window_info(hwnd) else {
            // Window vanished during drag — clean up placeholder.
            // Do NOT reinsert the window — it no longer exists.
            for ws_vec in self.workspaces.values_mut() {
                for ws in ws_vec.iter_mut() {
                    let _ = ws.remove_window(crate::state::DRAG_PLACEHOLDER_HWND);
                }
            }
            self.snap_back_tiled(drag.source_monitor, drag.source_workspace_idx);
            // No-op if hwnd is truly destroyed, but cheap and
            // covers the edge case where lookup returns None
            // transiently while the window still exists.
            leopardwm_platform_win32::set_dwm_transitions_disabled(hwnd, false);
            return;
        };
        let monitors: Vec<_> = self.monitors.values().cloned().collect();
        let target_monitor = find_monitor_for_rect(&monitors, &win_info.rect)
            .map(|m| m.id)
            .unwrap_or(drag.source_monitor);

        let shift_held = is_shift_key_pressed();

        if shift_held {
            for ws_vec in self.workspaces.values_mut() {
                for ws in ws_vec.iter_mut() {
                    let _ = ws.remove_window(crate::state::DRAG_PLACEHOLDER_HWND);
                }
            }
            let target_viewport = self.layout_viewport(target_monitor);
            let target_ws_idx = self.active_workspace_idx(target_monitor);
            let target_row = self
                .workspaces
                .get(&target_monitor)
                .and_then(|v| v.get(target_ws_idx))
                .map(|ws| {
                    let cursor_y = leopardwm_platform_win32::get_cursor_pos()
                        .map(|(_, y)| y)
                        .unwrap_or(win_info.rect.y + win_info.rect.height / 2);
                    crate::drag::target_row_for_cursor(ws, target_viewport, cursor_y)
                })
                .unwrap_or(0);

            // Shift+drop: column reorder on same row (already live-reordered),
            // or cross-row / cross-monitor column move.
            if target_monitor == drag.source_monitor && target_row == drag.source_row {
                self.snap_back_tiled(drag.source_monitor, drag.source_workspace_idx);
            } else {
                self.execute_cross_monitor_drag(
                    hwnd,
                    &drag,
                    target_monitor,
                    target_row,
                    &win_info.rect,
                );
            }
        } else {
            self.finalize_drag_merge(hwnd, &drag, target_monitor, &win_info.rect);
        }
        // Restore minimize/maximize transitions after placement; the temporary
        // DWM disable suppresses only the drop-to-layout slide.
        leopardwm_platform_win32::set_dwm_transitions_disabled(hwnd, false);
    }

    pub(crate) fn application_fullscreen_geometry(
        &self,
        hwnd: u64,
    ) -> (Option<Rect>, Option<Rect>) {
        let chrome_rect = leopardwm_platform_win32::get_window_chrome_rect(hwnd);
        let dwm_rect = chrome_rect
            .is_none()
            .then(|| leopardwm_platform_win32::get_window_visible_rect(hwnd))
            .flatten();
        (chrome_rect, dwm_rect)
    }

    pub(crate) fn observe_current_application_fullscreen(
        &self,
        hwnd: u64,
    ) -> Option<ApplicationFullscreenState> {
        let (chrome_rect, dwm_rect) = self.application_fullscreen_geometry(hwnd);
        self.observe_application_fullscreen(
            hwnd,
            chrome_rect,
            dwm_rect,
            leopardwm_platform_win32::is_window_maximized(hwnd),
        )
    }

    pub(crate) fn observe_application_fullscreen(
        &self,
        hwnd: u64,
        chrome_rect: Option<Rect>,
        dwm_rect: Option<Rect>,
        is_zoomed: bool,
    ) -> Option<ApplicationFullscreenState> {
        let (monitor_id, ws_idx) = self.find_window_workspace(hwnd)?;
        if self
            .workspaces
            .get(&monitor_id)
            .and_then(|workspaces| workspaces.get(ws_idx))
            .is_some_and(|workspace| workspace.fullscreen_window_id() == Some(hwnd))
        {
            return None;
        }
        let session = detect_application_fullscreen(
            self.monitors.values(),
            chrome_rect,
            dwm_rect,
            is_zoomed,
        )?;
        let expected_physical = self.expected_physical_rect(hwnd);
        if let Some(expected) = application_fullscreen_expected_layout_rect(
            expected_physical.or_else(|| self.compute_window_layout_rect(hwnd)),
            expected_physical.or_else(|| self.last_placed_layout_rects.get(&hwnd).copied()),
        ) {
            let scale_factor = self
                .monitors
                .get(&monitor_id)
                .map(|monitor| monitor.scale_factor)
                .unwrap_or(1.0);
            if chrome_rect.is_some_and(|rect| {
                chrome_rect_matches_layout_rect(
                    rect,
                    expected,
                    leopardwm_platform_win32::get_window_invisible_insets(hwnd),
                    scale_factor,
                )
            }) {
                return None;
            }
        }
        Some(session)
    }

    fn exit_application_fullscreen(&mut self, hwnd: u64) {
        if self.application_fullscreen.remove(&hwnd).is_none() {
            return;
        }
        self.last_placed_layout_rects.remove(&hwnd);
        info!("Application fullscreen window {} exited", hwnd);
        let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) else {
            return;
        };
        let is_floating = self
            .workspaces
            .get(&monitor_id)
            .and_then(|workspaces| workspaces.get(ws_idx))
            .is_some_and(|workspace| workspace.is_floating(hwnd));
        let route = application_fullscreen_exit_route(
            is_floating,
            leopardwm_platform_win32::is_window_maximized(hwnd),
            ws_idx == self.active_workspace_idx(monitor_id),
        );
        let restore_border = application_fullscreen_exit_restores_border(
            route,
            self.previous_focused_hwnd == Some(hwnd),
        );
        match route {
            ApplicationFullscreenExitRoute::FloatingPreserve => {
                if restore_border {
                    self.show_border(hwnd);
                }
            }
            ApplicationFullscreenExitRoute::MaximizedAllow => {
                self.window_last_maximized_at
                    .insert(hwnd, std::time::Instant::now());
                if restore_border {
                    self.show_border(hwnd);
                }
            }
            ApplicationFullscreenExitRoute::InactivePark => {
                #[cfg(not(test))]
                if !self.paused {
                    let _ = leopardwm_platform_win32::move_window_offscreen(hwnd);
                }
                self.sync_inactive_workspace_taskbar_button(hwnd);
            }
            ApplicationFullscreenExitRoute::ActiveTiledApply => {
                if let Err(e) = self.apply_layout() {
                    warn!(
                        "Failed to restore layout after application fullscreen exit: {}",
                        e
                    );
                }
            }
        }
        self.update_tab_strip();
    }

    fn reconcile_application_fullscreen_sessions(&mut self) {
        let tracked: Vec<_> = self.application_fullscreen.keys().copied().collect();
        for hwnd in tracked {
            let Some(stored) = self.application_fullscreen.get(&hwnd).copied() else {
                continue;
            };
            let is_valid = leopardwm_platform_win32::is_valid_window(hwnd);
            let is_managed = self.find_window_workspace(hwnd).is_some();
            let is_zoomed = is_valid && leopardwm_platform_win32::is_window_maximized(hwnd);
            let (chrome_rect, dwm_rect) = if is_valid {
                self.application_fullscreen_geometry(hwnd)
            } else {
                (None, None)
            };
            let observed = if is_managed {
                self.observe_application_fullscreen(hwnd, chrome_rect, dwm_rect, is_zoomed)
            } else {
                None
            };
            let scale_factor = self
                .monitors
                .get(&stored.monitor_id)
                .or_else(|| {
                    self.monitors
                        .values()
                        .find(|monitor| monitor.contains_rect_center(&stored.rect))
                })
                .map(|monitor| monitor.scale_factor)
                .unwrap_or(1.0);
            match application_fullscreen_reconciliation(
                is_valid,
                is_managed,
                is_zoomed,
                stored,
                observed,
                chrome_rect.or(dwm_rect),
                fullscreen_rect_tolerance(scale_factor),
            ) {
                ApplicationFullscreenReconciliation::Retain => {}
                ApplicationFullscreenReconciliation::Update => {
                    self.application_fullscreen.insert(hwnd, observed.unwrap());
                }
                ApplicationFullscreenReconciliation::Exit => self.exit_application_fullscreen(hwnd),
            }
        }
    }

    pub(crate) fn recheck_deferred_window_moves(&mut self) -> bool {
        let mut checked = false;
        let mut deferred = std::mem::take(&mut self.deferred_moved_or_resized);
        deferred.extend(
            self.window_move_recheck_attempts
                .keys()
                .copied()
                .filter(|hwnd| {
                    !self.is_managed_member(*hwnd)
                        || self
                            .current_physical_visibility(*hwnd)
                            .is_some_and(|visibility| visibility != Visibility::Visible)
                }),
        );
        let replay_windows = deferred.clone();
        deferred.extend(self.deferred_physical_landings.iter().copied());
        deferred.retain(|hwnd| {
            if self.is_managed_member(*hwnd) {
                return true;
            }
            self.window_move_recheck_attempts.remove(hwnd);
            self.deferred_physical_landings.remove(hwnd);
            false
        });
        let mut deferred = deferred.into_iter();
        while let Some(hwnd) = deferred.next() {
            let visibility = self.current_physical_visibility(hwnd);
            let location = self.find_window_workspace(hwnd);
            let inactive_replay = replay_windows.contains(&hwnd)
                && location.is_some_and(|(monitor_id, ws_idx)| {
                    ws_idx != self.active_workspace_idx(monitor_id)
                });
            let workspace = location
                .and_then(|(monitor_id, ws_idx)| self.workspaces.get(&monitor_id)?.get(ws_idx));
            if (visibility.is_none() && !inactive_replay)
                || workspace.is_none_or(|ws| ws.is_minimized(hwnd))
            {
                self.window_move_recheck_attempts.remove(&hwnd);
                self.deferred_physical_landings.remove(&hwnd);
                continue;
            }
            if !replay_windows.contains(&hwnd)
                && (leopardwm_platform_win32::pending_async_position(hwnd)
                    || defer_snapback_while_settling(
                        self.window_managed_at.get(&hwnd).copied(),
                        self.window_last_maximized_at.get(&hwnd).copied(),
                        std::time::Instant::now(),
                    ))
            {
                continue;
            }
            if self.applying_layout
                || self.display_change_pending
                || self.should_suppress_moved_or_resized(hwnd)
            {
                if replay_windows.contains(&hwnd) {
                    self.deferred_moved_or_resized.insert(hwnd);
                }
                continue;
            }
            let finalize_landing = !inactive_replay
                && self.deferred_physical_landings.contains(&hwnd)
                && !leopardwm_platform_win32::pending_async_position(hwnd);
            let before = self.physical_request_seq;
            if finalize_landing {
                self.last_placed_layout_rects.remove(&hwnd);
                if let Err(error) = self.apply_layout() {
                    warn!(
                        "Failed to finalize deferred placement of window {}: {}",
                        hwnd, error
                    );
                }
            } else {
                self.deferred_physical_landings.remove(&hwnd);
                self.on_window_moved_or_resized(hwnd, true);
            }
            if self.physical_request_seq != before {
                checked = true;
                self.deferred_moved_or_resized
                    .extend(deferred.filter(|hwnd| replay_windows.contains(hwnd)));
                break;
            }
        }
        checked
    }

    fn on_window_moved_or_resized(&mut self, hwnd: u64, periodic_recheck: bool) {
        if self.size_ignored_windows.remove(&hwnd) {
            self.try_admit_window_at(hwnd, AdmissionKind::Automatic, None);
            return;
        }
        #[cfg(test)]
        let maximized = self.injected_window_maximized.get(&hwnd).copied();
        #[cfg(test)]
        let press_count = self.native_user_press_count();
        self.on_window_moved_or_resized_with_native_ops(
            hwnd,
            periodic_recheck,
            || {
                #[cfg(test)]
                {
                    press_count
                }
                #[cfg(not(test))]
                leopardwm_platform_win32::user_press_count()
            },
            leopardwm_platform_win32::queue_maximized_window_restore,
            |window_id| {
                #[cfg(test)]
                if let Some(maximized) = maximized {
                    return maximized;
                }
                leopardwm_platform_win32::is_window_maximized(window_id)
            },
        );
    }

    pub(crate) fn on_window_moved_or_resized_with_native_ops(
        &mut self,
        hwnd: u64,
        periodic_recheck: bool,
        mut press_count: impl FnMut() -> Option<u64>,
        mut queue_maximized_restore: impl FnMut(
            u64,
        )
            -> Result<bool, leopardwm_platform_win32::Win32Error>,
        mut is_maximized: impl FnMut(u64) -> bool,
    ) {
        if self.pending_maximized_admission_restores.contains(&hwnd) {
            return;
        }
        if self
            .post_admission_maximize_restore_eligible
            .contains_key(&hwnd)
            && !self
                .window_managed_at
                .get(&hwnd)
                .is_some_and(|managed_at| managed_at.elapsed() < SNAPBACK_SETTLE_AFTER_CREATE)
        {
            self.post_admission_maximize_restore_eligible.remove(&hwnd);
        }
        let managed_tiled = self
            .find_window_workspace(hwnd)
            .and_then(|(monitor_id, ws_idx)| {
                self.workspaces
                    .get(&monitor_id)?
                    .get(ws_idx)
                    .map(|workspace| {
                        !workspace.is_floating(hwnd)
                            && workspace.fullscreen_window_id() != Some(hwnd)
                    })
            })
            .unwrap_or(false);
        if managed_tiled
            && self
                .post_admission_maximize_restore_eligible
                .contains_key(&hwnd)
            && !self.is_application_fullscreen(hwnd)
        {
            let native_maximized = is_maximized(hwnd);
            let (chrome_rect, dwm_rect) = self.application_fullscreen_geometry(hwnd);
            let session =
                self.observe_application_fullscreen(hwnd, chrome_rect, dwm_rect, native_maximized);
            if session.is_some() {
                self.post_admission_maximize_restore_eligible.remove(&hwnd);
            } else if native_maximized
                && self.try_restore_post_admission_maximize(
                    hwnd,
                    &mut press_count,
                    &mut queue_maximized_restore,
                    &mut is_maximized,
                )
            {
                // on_maximized_admission_restored applies layout; no deferred replay is needed.
                return;
            }
        }
        // Placement feedback stays suppressed, except a direct maximize of a
        // managed tiled window needs its timestamp and target-only visual cleanup
        // immediately so the later restore is classified correctly.
        if self.applying_layout || self.display_change_pending {
            if self.find_window_workspace(hwnd).is_some() {
                self.deferred_moved_or_resized.insert(hwnd);
            }
            let is_maximized = is_maximized(hwnd);
            if should_observe_maximize_during_suppression(
                self.applying_layout,
                self.display_change_pending,
                managed_tiled,
                self.is_application_fullscreen(hwnd),
                is_maximized,
            ) {
                self.observe_tiled_window_maximized(hwnd);
            }
            return;
        }
        let (chrome_rect, dwm_rect) = self.application_fullscreen_geometry(hwnd);
        let session =
            self.observe_application_fullscreen(hwnd, chrome_rect, dwm_rect, is_maximized(hwnd));
        let prior = self.application_fullscreen.get(&hwnd).copied();
        let lifecycle = application_fullscreen_lifecycle(prior, session);
        match lifecycle {
            ApplicationFullscreenLifecycle::Enter | ApplicationFullscreenLifecycle::Reassign => {
                let session = session.expect("fullscreen lifecycle requires a session");
                self.application_fullscreen.insert(hwnd, session);
                self.stop_ghosting_window(hwnd);
                if self.previous_focused_hwnd == Some(hwnd) {
                    self.hide_border();
                }
                self.update_tab_strip();
                match prior {
                    Some(old) => info!(
                        "Application fullscreen window {} reassigned from monitor {} to {}",
                        hwnd, old.monitor_id, session.monitor_id
                    ),
                    None => info!(
                        "Application fullscreen window {} entered on monitor {}",
                        hwnd, session.monitor_id
                    ),
                }
                return;
            }
            ApplicationFullscreenLifecycle::Continue => return,
            ApplicationFullscreenLifecycle::Exit => {
                self.exit_application_fullscreen(hwnd);
                return;
            }
            ApplicationFullscreenLifecycle::None => {}
        }
        if moved_or_resized_decision(lifecycle, self.should_suppress_moved_or_resized(hwnd))
            == MovedOrResizedDecision::Suppress
        {
            if self.find_window_workspace(hwnd).is_some() {
                self.deferred_moved_or_resized.insert(hwnd);
            }
            return;
        }
        if self.resize_hwnd == Some(hwnd) {
            let is_floating = self
                .find_window_workspace(hwnd)
                .and_then(|(mid, wsi)| {
                    self.workspaces
                        .get(&mid)?
                        .get(wsi)
                        .map(|ws| ws.is_floating(hwnd))
                })
                .unwrap_or(false);
            if is_floating {
                // Throttle floating border updates to ~60fps
                let now = std::time::Instant::now();
                if self
                    .last_resize_hint_update
                    .is_some_and(|t| now.duration_since(t).as_millis() < 16)
                {
                    return;
                }
                self.last_resize_hint_update = Some(now);
                self.show_border(hwnd);
            } else if self.config.snap_hints.enabled {
                // Throttle preview updates to ~60fps
                let now = std::time::Instant::now();
                if self
                    .last_resize_hint_update
                    .is_some_and(|t| now.duration_since(t).as_millis() < 16)
                {
                    return;
                }
                self.last_resize_hint_update = Some(now);
                self.update_resize_preview(hwnd);
            }
            return;
        }
        if let Some(ref mut drag) = self.drag_state {
            if drag.hwnd == hwnd {
                if drag.is_tiled {
                    // Throttle hint updates to ~60fps
                    let now = std::time::Instant::now();
                    if drag
                        .last_hint_update
                        .is_some_and(|t| now.duration_since(t).as_millis() < 16)
                    {
                        return;
                    }
                    drag.last_hint_update = Some(now);
                    self.update_drag_hint(hwnd);
                } else {
                    // Floating window drag — throttle border updates to ~60fps
                    let now = std::time::Instant::now();
                    if drag
                        .last_hint_update
                        .is_some_and(|t| now.duration_since(t).as_millis() < 16)
                    {
                        return;
                    }
                    drag.last_hint_update = Some(now);
                    self.show_border(hwnd);
                }
                return;
            }
        }
        if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
            if self.on_inactive_workspace_window_moved(hwnd, monitor_id, ws_idx, &mut is_maximized)
            {
                return;
            }
            let workspace = &self.workspaces[&monitor_id][ws_idx];
            let is_minimized = workspace.is_minimized(hwnd);

            if workspace.is_floating(hwnd) {
                self.on_floating_window_moved_or_resized(
                    hwnd,
                    monitor_id,
                    ws_idx,
                    is_maximized(hwnd),
                    periodic_recheck,
                );
            } else if is_maximized(hwnd) {
                // User maximized a tiled window — let it stay maximized. Record
                // the maximize so a brief restore mid-burst is treated as
                // settling rather than a snap-back trigger, and remove only
                // this target's ghost/crossfade visual immediately.
                self.observe_tiled_window_maximized(hwnd);
                debug!("Tiled window {} maximized — allowing", hwnd);
            } else if is_minimized {
                self.window_last_maximized_at.remove(&hwnd);
                debug!("Ignoring MovedOrResized for minimized window {}", hwnd);
            } else if defer_snapback_while_settling(
                self.window_managed_at.get(&hwnd).copied(),
                self.window_last_maximized_at.get(&hwnd).copied(),
                std::time::Instant::now(),
            ) {
                debug!("Deferring snap-back for settling maximized window {}", hwnd);
            } else {
                self.window_last_maximized_at.remove(&hwnd);
                // Position-based false-positive filter: EVENT_OBJECT_LOCATIONCHANGE
                // fires for many reasons besides actual movement (Z-order,
                // DWM composition, focus shuffles, DPI nudges, app-internal
                // size adjustments). Under CPU pressure these spurious
                // events trigger cascading full retiles. If the window's
                // current visible bounds are close to the last-placed
                // layout rect, skip the snap-back.
                //
                // Epsilon is generous (20px) because some apps report
                // their own content rect rather than the requested frame
                // rect — DPI rounding, custom chrome, internal min-sizes
                // all create small legitimate deltas we don't want to
                // chase. Real user drags are typically tens to hundreds
                // of pixels off, so 20px comfortably separates them.
                const POSITION_EPSILON_PX: i32 = 20;
                let expected = self
                    .expected_physical_rect(hwnd)
                    .or_else(|| self.last_placed_layout_rects.get(&hwnd).copied());
                let dwm_actual = leopardwm_platform_win32::get_window_visible_rect(hwnd);
                // Under the Chromium / Firefox / Cascadia swap-chain-stale bug,
                // EXTENDED_FRAME_BOUNDS can report a stale compositor position while
                // GetWindowRect still reports the actual HWND position.
                //
                // Placement subtracts invisible-border insets from the layout origin;
                // add them to the chrome origin before comparing, or displacements in
                // the 21..(20+inset_l*2) px band can be mistaken for compositor artifacts.
                let chrome_actual = leopardwm_platform_win32::get_window_chrome_rect(hwnd);
                let offscreen = self
                    .current_physical_visibility(hwnd)
                    .is_some_and(|visibility| visibility != Visibility::Visible);
                let chrome_visible = chrome_actual.filter(|_| !offscreen).map(|c| {
                    let (il, it, _, _) =
                        leopardwm_platform_win32::get_window_invisible_insets(hwnd);
                    Rect::new(c.x + il, c.y + it, c.width, c.height)
                });
                let within_all = |a: Rect, e: Rect, eps: i32| -> bool {
                    (a.x - e.x).abs() <= eps
                        && (a.y - e.y).abs() <= eps
                        && (a.width - e.width).abs() <= eps
                        && (a.height - e.height).abs() <= eps
                };
                let recheck_target = expected.zip(self.current_physical_visibility(hwnd));
                let at_expected_position = if offscreen {
                    // Compare the exact position and retained size resolved by placement, not fresh insets.
                    leopardwm_platform_win32::offscreen_placement_matches(hwnd) == Some(true)
                } else {
                    match expected {
                        Some(expected) => {
                            let dwm_ok = dwm_actual
                                .is_some_and(|a| within_all(a, expected, POSITION_EPSILON_PX));
                            // Swap-chain bug guard — chrome HWND
                            // (visible-area-corrected) is at the
                            // expected position even though DWM is
                            // lying. Position only: the chrome rect's
                            // size is inflated by invisible borders
                            // and we don't trivially correct that, so
                            // a size comparison would mask real edge
                            // resizes.
                            let chrome_position_ok = chrome_visible.is_some_and(|a| {
                                (a.x - expected.x).abs() <= POSITION_EPSILON_PX
                                    && (a.y - expected.y).abs() <= POSITION_EPSILON_PX
                            });
                            let dwm_position_displaced = dwm_actual.is_some_and(|a| {
                                (a.x - expected.x).abs() > POSITION_EPSILON_PX
                                    || (a.y - expected.y).abs() > POSITION_EPSILON_PX
                            });
                            let swap_chain_bug = chrome_position_ok && dwm_position_displaced;
                            let result = dwm_ok || swap_chain_bug;
                            if !result {
                                debug!(
                                "Window {} off expected position: expected {:?} dwm {:?} chrome_visible {:?}",
                                hwnd, expected, dwm_actual, chrome_visible
                            );
                            }
                            result
                        }
                        None => false,
                    }
                };
                if at_expected_position {
                    // Off-screen landings end map-driven retries; visible bounds survive only an unchanged target.
                    if offscreen
                        || self
                            .window_move_recheck_attempts
                            .get(&hwnd)
                            .is_some_and(|attempt| Some(attempt.target) != recheck_target)
                    {
                        self.window_move_recheck_attempts.remove(&hwnd);
                    }
                    debug!(
                        "Ignoring spurious MovedOrResized for {} — already at expected layout position",
                        hwnd
                    );
                } else {
                    if periodic_recheck {
                        let Some(target) = recheck_target else {
                            return;
                        };
                        if let Some(attempt) = self.window_move_recheck_attempts.get_mut(&hwnd) {
                            let recent =
                                attempt.issued_at.elapsed() <= WINDOW_MOVE_RECHECK_FIGHT_WINDOW;
                            if attempt.target == target && (offscreen || attempt.gave_up || recent)
                            {
                                if !attempt.gave_up {
                                    debug!(
                                        "Giving up deferred MovedOrResized for {} — target {:?} already corrected",
                                        hwnd, target
                                    );
                                    attempt.gave_up = true;
                                }
                                return;
                            }
                        }
                        self.window_move_recheck_attempts.insert(
                            hwnd,
                            WindowMoveRecheckAttempt {
                                target,
                                issued_at: std::time::Instant::now(),
                                gave_up: false,
                            },
                        );
                    }
                    debug!("Managed window {} moved/resized — snapping back", hwnd);
                    // Evict the displaced hwnd's last-applied entry so
                    // apply_layout's fast-path can't short-circuit when
                    // the layout itself hasn't changed but the window's
                    // visible rect has drifted away from it. Without
                    // this the window stays where the user dragged it.
                    self.last_placed_layout_rects.remove(&hwnd);
                    if offscreen {
                        // Off-screen HWNDs have no last-placed layout entry to evict.
                        self.bump_physical_invalidation();
                    }
                    if let Err(e) = self.apply_layout() {
                        warn!("Failed to snap back layout after move/resize: {}", e);
                    }
                    if offscreen
                        && leopardwm_platform_win32::offscreen_placement_matches(hwnd) == Some(true)
                    {
                        self.window_move_recheck_attempts.remove(&hwnd);
                    }
                }
            }
        }
    }

    fn on_inactive_workspace_window_moved(
        &mut self,
        hwnd: u64,
        monitor_id: leopardwm_platform_win32::MonitorId,
        ws_idx: usize,
        is_maximized: &mut impl FnMut(u64) -> bool,
    ) -> bool {
        let workspace = &self.workspaces[&monitor_id][ws_idx];
        if ws_idx == self.active_workspace_idx(monitor_id)
            || self.sticky_windows.contains(&hwnd)
            || workspace.fullscreen_window_id() == Some(hwnd)
            || workspace.is_minimized(hwnd)
            || is_maximized(hwnd)
        {
            return false;
        }
        if self.layout_transition.is_some() {
            self.deferred_moved_or_resized.insert(hwnd);
            return true;
        }
        self.window_move_recheck_attempts.remove(&hwnd);
        self.window_last_maximized_at.remove(&hwnd);
        if leopardwm_platform_win32::get_window_chrome_rect(hwnd)
            .is_some_and(|rect| !leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect))
            && self.park_inactive_workspace_window(hwnd)
        {
            debug!(
                "Managed window {} on an inactive workspace moved back on screen; parking it again",
                hwnd
            );
        }
        true
    }

    fn on_floating_window_moved_or_resized(
        &mut self,
        hwnd: u64,
        monitor_id: leopardwm_platform_win32::MonitorId,
        ws_idx: usize,
        is_maximized: bool,
        deferred: bool,
    ) {
        self.window_move_recheck_attempts.remove(&hwnd);
        if self.previous_focused_hwnd == Some(hwnd) {
            self.show_border(hwnd);
        }
        if self.layout_transition.is_some() {
            self.deferred_moved_or_resized.insert(hwnd);
        }
        if let Some(rect) =
            self.floating_move_geometry(hwnd, monitor_id, ws_idx, is_maximized, deferred)
        {
            self.workspaces.get_mut(&monitor_id).unwrap()[ws_idx].update_floating(hwnd, rect);
        }
    }

    fn floating_move_geometry(
        &self,
        hwnd: u64,
        monitor_id: leopardwm_platform_win32::MonitorId,
        ws_idx: usize,
        is_maximized: bool,
        deferred: bool,
    ) -> Option<Rect> {
        let workspace = &self.workspaces[&monitor_id][ws_idx];
        if self.applying_layout
            || self.display_change_pending
            || self.layout_transition.is_some()
            || self.resize_hwnd == Some(hwnd)
            || self
                .drag_state
                .as_ref()
                .is_some_and(|drag| drag.hwnd == hwnd)
            || self.is_application_fullscreen(hwnd)
            || ws_idx != self.active_workspace_idx(monitor_id)
            || workspace.is_minimized(hwnd)
            || leopardwm_platform_win32::window_minimized_state(hwnd) == Some(true)
            || is_maximized
            || workspace.fullscreen_window_id() == Some(hwnd)
            || self
                .current_physical_visibility(hwnd)
                .is_some_and(|visibility| visibility != Visibility::Visible)
            || leopardwm_platform_win32::is_placement_parked(hwnd)
            || leopardwm_platform_win32::get_window_chrome_rect(hwnd).is_some_and(|rect| {
                leopardwm_platform_win32::is_move_offscreen_sentinel_rect(&rect)
            })
        {
            return None;
        }
        let actual = leopardwm_platform_win32::get_window_visible_rect(hwnd)?;
        if let Some(acknowledged) = self.acknowledged_visible_rect(hwnd) {
            let floating = workspace
                .floating_windows()
                .iter()
                .find(|floating| floating.id == hwnd)?;
            // A clamped landing is feedback only until the stored placement target changes.
            if self.expected_physical_rect(hwnd) == Some(floating.rect)
                && actual.x.abs_diff(acknowledged.x) <= 2
                && actual.y.abs_diff(acknowledged.y) <= 2
                && actual.width.abs_diff(acknowledged.width) <= 2
                && actual.height.abs_diff(acknowledged.height) <= 2
            {
                return None;
            }
        } else if deferred {
            return None;
        }
        Some(actual)
    }

    pub(crate) fn preserve_deferred_floating_moves(&mut self) {
        if self.post_animation_nudge_pending || self.pending_idle_layout_reapply {
            return;
        }
        let deferred: Vec<_> = self.deferred_moved_or_resized.iter().copied().collect();
        for hwnd in deferred {
            let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) else {
                continue;
            };
            let Some(floating) = self.workspaces[&monitor_id][ws_idx]
                .floating_windows()
                .iter()
                .find(|floating| floating.id == hwnd)
            else {
                continue;
            };
            let Some(expected) = self.expected_physical_rect(hwnd) else {
                continue;
            };
            // A changed stored target belongs to a command, not stale placement
            // feedback. Only harvest displacement from an already-landed target.
            if floating.rect != expected {
                continue;
            }
            let Some(actual) = self.floating_move_geometry(
                hwnd,
                monitor_id,
                ws_idx,
                self.native_window_is_maximized(hwnd),
                true,
            ) else {
                continue;
            };
            self.workspaces.get_mut(&monitor_id).unwrap()[ws_idx].update_floating(hwnd, actual);
        }
    }

    fn on_display_change(&mut self) {
        // Inset-cache clearing and high-contrast refresh run on WM_DISPLAYCHANGE
        // receipt, before this debounced reconciliation.
        info!("Display configuration changed - reconciling monitors");
        let enumerated: Result<Vec<MonitorInfo>, leopardwm_platform_win32::Win32Error> = {
            #[cfg(test)]
            {
                match self.injected_display_monitors.clone() {
                    Some(monitors) => Ok(monitors),
                    None => {
                        warn!("Ignoring display change in tests without injected monitors");
                        return;
                    }
                }
            }
            #[cfg(not(test))]
            {
                enumerate_monitors()
            }
        };
        match enumerated {
            Ok(new_monitors) if !new_monitors.is_empty() => {
                info!(
                    "Detected {} monitor(s) after display change",
                    new_monitors.len()
                );
                for m in &new_monitors {
                    info!(
                        "  Monitor {}: {}x{} at ({},{}){} \"{}\"",
                        m.id,
                        m.work_area.width,
                        m.work_area.height,
                        m.work_area.x,
                        m.work_area.y,
                        if m.is_primary { " [PRIMARY]" } else { "" },
                        m.device_name
                    );
                }

                self.reconcile_monitors(new_monitors);
                self.reconcile_application_fullscreen_sessions();

                // Correct minimized flags before deciding whether foreground can be adopted.
                self.resync_minimized_from_os();

                // Topology changes can move the foreground window without a focus event.
                #[cfg(test)]
                let foreground = self.injected_foreground_hwnd.flatten();
                #[cfg(not(test))]
                let foreground = leopardwm_platform_win32::get_foreground_window();
                let managed_foreground = foreground.filter(|_| !self.paused).and_then(|hwnd| {
                    let (monitor_id, ws_idx) = self.find_window_workspace(hwnd)?;
                    let workspace = self.workspaces.get(&monitor_id)?.get(ws_idx)?;
                    if workspace.is_minimized(hwnd)
                        || fullscreen_focus_guard(false, workspace.fullscreen_window_id(), hwnd)
                            .is_some()
                    {
                        return None;
                    }
                    if self.observe_current_application_fullscreen(hwnd).is_some()
                        || self.is_application_fullscreen(hwnd)
                        || self.application_fullscreen.iter().any(|(&other, session)| {
                            other != hwnd && session.monitor_id == monitor_id
                        })
                    {
                        return None;
                    }
                    Some((hwnd, monitor_id, ws_idx))
                });
                if let Some((hwnd, monitor_id, ws_idx)) = managed_foreground {
                    self.follow_workspace_without_stealing_focus(monitor_id, ws_idx, None);
                    if let Some(ref mut transition) = self.layout_transition {
                        transition.suppress_landing_focus_resync = true;
                    }
                    if let Some(workspace) = self
                        .workspaces
                        .get_mut(&monitor_id)
                        .and_then(|v| v.get_mut(ws_idx))
                    {
                        if let Err(e) = workspace.focus_window(hwnd) {
                            debug!(
                                "Failed to focus window {} after display change: {}",
                                hwnd, e
                            );
                        }
                    }
                    self.previous_focused_hwnd = Some(hwnd);
                }

                // Adopt foreground first so its workspace is not parked as inactive.
                self.park_inactive_workspace_windows();

                self.display_change_apply_in_progress = true;
                let result = self.apply_layout();
                self.display_change_apply_in_progress = false;
                if let Err(e) = result {
                    warn!("Failed to apply layout after display change: {}", e);
                }
                if let Some((hwnd, monitor_id, _)) = managed_foreground {
                    self.show_border(hwnd);
                    self.broadcast_focused_window_if_changed(monitor_id as i64, Some(hwnd));
                }
                self.sync_taskbar_buttons();
            }
            Ok(_) => {
                warn!("No monitors found after display change");
            }
            Err(e) => {
                warn!("Failed to enumerate monitors after display change: {}", e);
            }
        }
    }

    /// Snap-target changes request a vsync-aligned animation thread so the
    /// preview interpolates smoothly between presets.
    fn update_resize_preview(&mut self, hwnd: u64) {
        let Some(visible_rect) = leopardwm_platform_win32::get_window_visible_rect(hwnd) else {
            return;
        };
        let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) else {
            return;
        };
        let is_floating = self
            .workspaces
            .get(&monitor_id)
            .and_then(|v| v.get(ws_idx))
            .is_none_or(|ws| ws.is_floating(hwnd));
        if is_floating {
            return;
        }

        let work_area = match self.monitors.get(&monitor_id) {
            Some(m) => m.work_area,
            None => return,
        };
        let width_presets = self.config.layout.width_presets.clone();
        let height_presets = self.config.layout.height_presets.clone();

        let snap_rect = self
            .workspaces
            .get_mut(&monitor_id)
            .and_then(|v| v.get_mut(ws_idx))
            .and_then(|ws| {
                ws.preview_resize_snap(
                    hwnd,
                    visible_rect.width,
                    visible_rect.height,
                    &width_presets,
                    &height_presets,
                    work_area,
                )
            });

        let Some(target_rect) = snap_rect else {
            return;
        };

        if self.resize_preview_target == Some(target_rect) {
            // Target unchanged — if animation thread is driving the overlay, let it.
            if !self
                .resize_animation_active
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                self.pending_drag_hint =
                    Some(crate::state::DragHintAction::ShowGhost { rect: target_rect });
            }
            self.resize_preview_display_rect = Some(target_rect);
            self.show_border(hwnd);
            return;
        }

        let start_rect = self.resize_preview_display_rect.unwrap_or(target_rect);
        self.resize_preview_target = Some(target_rect);
        self.resize_preview_display_rect = Some(start_rect);
        self.pending_resize_animation = Some(crate::state::ResizeAnimationRequest {
            start_rect,
            target_rect,
        });

        self.pending_drag_hint = Some(crate::state::DragHintAction::ShowGhost { rect: start_rect });
        self.show_border(hwnd);
    }

    fn handle_resize_complete(&mut self, hwnd: u64) {
        #[cfg(test)]
        self.resize_complete_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.teardown_resize_preview_ui();
        let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) else {
            let _ = self.apply_layout();
            return;
        };

        let is_floating = self
            .workspaces
            .get(&monitor_id)
            .and_then(|v| v.get(ws_idx))
            .is_none_or(|ws| ws.is_floating(hwnd));

        if is_floating {
            if let Some(visible_rect) = leopardwm_platform_win32::get_window_visible_rect(hwnd) {
                if let Some(ws) = self
                    .workspaces
                    .get_mut(&monitor_id)
                    .and_then(|v| v.get_mut(ws_idx))
                {
                    ws.update_floating(hwnd, visible_rect);
                }
            }
            return;
        }

        let Some(visible_rect) = leopardwm_platform_win32::get_window_visible_rect(hwnd) else {
            let _ = self.apply_layout();
            return;
        };

        let viewport_width = self.viewport_width_for(monitor_id);
        let width_presets = self.config.layout.width_presets.clone();
        let height_presets = self.config.layout.height_presets.clone();

        if let Some(ws) = self
            .workspaces
            .get_mut(&monitor_id)
            .and_then(|v| v.get_mut(ws_idx))
        {
            if let Some((row_idx, col_idx, win_idx)) = ws.find_window_location_rc(hwnd) {
                ws.snap_column_width_to_preset_in_row(
                    row_idx,
                    col_idx,
                    visible_rect.width,
                    &width_presets,
                    viewport_width,
                );

                let col_len = ws
                    .column_for_row(row_idx, col_idx)
                    .map(|c| c.len())
                    .unwrap_or(0);
                if col_len > 1 {
                    let viewport_height = self
                        .monitors
                        .get(&monitor_id)
                        .map(|m| m.work_area.height)
                        .unwrap_or(crate::state::FALLBACK_WORK_AREA_HEIGHT);
                    ws.snap_window_height_to_preset_in_row(
                        row_idx,
                        col_idx,
                        win_idx,
                        visible_rect.height,
                        &height_presets,
                        viewport_height,
                    );
                }

                ws.ensure_focused_visible_animated(viewport_width);

                info!(
                    "Resize snap: window {} → width preset, new column width = {}",
                    hwnd,
                    ws.column_for_row(row_idx, col_idx)
                        .map(|c| c.width())
                        .unwrap_or(0)
                );
            }
        }

        // Evict the resized hwnd from last_placed_layout_rects: when the
        // user's resize falls inside the current preset bucket the snap is
        // a no-op, so apply_layout's fast-path would see placements
        // unchanged and skip repositioning, leaving the window at the
        // user-resized size instead of the column's preset width.
        self.last_placed_layout_rects.remove(&hwnd);
        if let Err(e) = self.apply_layout() {
            warn!("Failed to apply layout after resize snap: {}", e);
        } else {
            self.sync_taskbar_buttons();
        }
    }

    pub(crate) fn apply_focus_follows_mouse(&mut self, hwnd: u64) -> bool {
        if leopardwm_platform_win32::focus_placeholder::is_focus_placeholder(hwnd) {
            return false;
        }
        if let Some((monitor_id, ws_idx)) = self.find_window_workspace(hwnd) {
            self.focused_monitor = monitor_id;

            let viewport_width = self.viewport_width_for(monitor_id);

            if let Some(workspace) = self
                .workspaces
                .get_mut(&monitor_id)
                .and_then(|v| v.get_mut(ws_idx))
            {
                if workspace.is_floating(hwnd) {
                    self.previous_focused_hwnd = Some(hwnd);
                    // Draw the focus border here. Pre-setting previous_focused_hwnd
                    // above makes the OS-side EVENT_SYSTEM_FOREGROUND dedup at the
                    // top of on_window_focused early-return, so it never paints the
                    // border (or broadcasts) for this window — we must do both here
                    // or the floating window looks unfocused.
                    self.show_border(hwnd);
                    // Skip the real Win32 call in tests — placeholder hwnds collide
                    // with real running windows and lag the user's mouse / steal
                    // focus via AttachThreadInput.
                    #[cfg(not(test))]
                    let _ = leopardwm_platform_win32::set_foreground_window(hwnd);
                    debug!(
                        "Focus-follows-mouse: focused floating window {} on monitor {}",
                        hwnd, monitor_id
                    );
                    self.broadcast_focused_window_if_changed(monitor_id as i64, Some(hwnd));
                    return true;
                }
                if let Err(e) = workspace.focus_window(hwnd) {
                    debug!(
                        "Failed to focus window {} for focus-follows-mouse: {}",
                        hwnd, e
                    );
                    return false;
                }
                debug!(
                    "Focus-follows-mouse: focused window {} on monitor {}",
                    hwnd, monitor_id
                );
                workspace.ensure_focused_visible_animated(viewport_width);
                if let Err(e) = self.apply_layout() {
                    warn!("Failed to apply layout after focus-follows-mouse: {}", e);
                }
                // Drop any floating-window preference left by a prior hover so
                // the tiled focus actually takes effect — otherwise
                // sync_foreground_window keeps foregrounding the still-floating
                // previous window and the tiled window never gets focus.
                self.previous_focused_hwnd = None;
                self.sync_foreground_window();
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod snapback_settle_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn observes_direct_managed_maximize_during_apply_without_releasing_ordinary_suppression() {
        assert!(should_observe_maximize_during_suppression(
            true, false, true, false, true
        ));
        assert!(!should_observe_maximize_during_suppression(
            true, false, false, false, true
        ));
        assert!(!should_observe_maximize_during_suppression(
            true, false, true, true, true
        ));
        assert!(!should_observe_maximize_during_suppression(
            true, true, true, false, true
        ));
        assert!(!should_observe_maximize_during_suppression(
            true, false, true, false, false
        ));
        assert!(!should_observe_maximize_during_suppression(
            false, false, true, false, true
        ));
    }

    #[test]
    fn defers_only_while_recently_created_and_recently_maximized() {
        let now = Instant::now();
        let fresh = now - Duration::from_millis(500);
        let stale_create = now - (SNAPBACK_SETTLE_AFTER_CREATE + Duration::from_millis(100));
        let stale_max = now - (SNAPBACK_MAXIMIZE_GRACE + Duration::from_millis(100));

        assert!(defer_snapback_while_settling(Some(fresh), Some(fresh), now));
        assert!(!defer_snapback_while_settling(
            Some(stale_create),
            Some(fresh),
            now
        ));
        assert!(!defer_snapback_while_settling(
            Some(fresh),
            Some(stale_max),
            now
        ));
        assert!(!defer_snapback_while_settling(Some(fresh), None, now));
        assert!(!defer_snapback_while_settling(None, Some(fresh), now));
    }
}

#[cfg(test)]
mod edit_config_match_tests {
    use super::title_names_config_file;

    #[test]
    fn matches_editor_titles_as_a_whole_token() {
        assert!(title_names_config_file(
            "config.toml - Visual Studio Code",
            "config.toml"
        ));
        assert!(title_names_config_file(
            "config.toml — Sublime Text",
            "config.toml"
        ));
        assert!(title_names_config_file(
            r"C:\Users\Jose\AppData\config.toml - Notepad++",
            "config.toml"
        ));
        assert!(title_names_config_file("config.toml", "config.toml"));
        assert!(title_names_config_file(
            "CONFIG.TOML - Editor",
            "config.toml"
        ));
    }

    #[test]
    fn rejects_substring_of_a_longer_word() {
        assert!(!title_names_config_file(
            "myconfig.toml - Editor",
            "config.toml"
        ));
        assert!(!title_names_config_file(
            "config.toml.bak - Editor",
            "config.toml"
        ));
        assert!(!title_names_config_file("Inbox - Mail", "config.toml"));
        assert!(!title_names_config_file("config.toml", ""));
    }
}
