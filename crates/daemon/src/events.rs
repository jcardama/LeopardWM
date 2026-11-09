use leopardwm_platform_win32::overview::OverviewEvent;
use leopardwm_platform_win32::tab_strip::TabAction;
use leopardwm_platform_win32::{GestureEvent, HotkeyEvent, Modifiers, WindowEvent};
use std::collections::BTreeSet;
use tokio::sync::{broadcast, oneshot};

use crate::animation_worker;
use crate::settings;
use crate::tray;
use leopardwm_ipc::{EventKind, IpcCommand, IpcEvent, IpcResponse};

/// Bundle returned to the per-client IPC task when a `Subscribe` command
/// is processed. Created atomically under the AppState mutex so the
/// snapshot reflects state at the same instant the broadcast receiver
/// was subscribed — no event between subscribe and snapshot can be lost.
pub(crate) struct SubscribeStartup {
    /// The `IpcResponse::Subscribed` ack frame to write to the client first.
    pub(crate) ack: IpcResponse,
    /// Initial snapshot frames to write after the ack.
    pub(crate) snapshot: Vec<IpcEvent>,
    /// Receiver attached to the matching legacy-only or workspace-enabled
    /// broadcaster; delivers requested events sent after the snapshot was taken.
    pub(crate) receiver: broadcast::Receiver<IpcEvent>,
}

pub(crate) enum DaemonEvent {
    IpcCommand {
        cmd: IpcCommand,
        responder: oneshot::Sender<IpcResponse>,
    },
    /// An IPC `Subscribe` command. Handled separately from `IpcCommand`
    /// because the response carries a `broadcast::Receiver` and a
    /// snapshot, which the regular `IpcResponse` channel can't express.
    /// The main loop processes this under the AppState mutex so the
    /// snapshot + receiver creation are atomic.
    IpcSubscribe {
        events: BTreeSet<EventKind>,
        responder: oneshot::Sender<SubscribeStartup>,
    },
    WindowEvent(WindowEvent),
    Hotkey(HotkeyEvent),
    RecordedHotkey {
        modifiers: Modifiers,
        vk: u32,
    },
    Gesture(GestureEvent),
    Tray(tray::TrayEvent),
    Settings(settings::SettingsEvent),
    /// An animation frame was applied by the worker thread.
    AnimationFrameApplied(animation_worker::FrameResult),
    /// A worker-driven crossfade target was actually dropped. Daemon releases
    /// only that target's same-source re-registration barrier.
    CrossfadeTargetDropped {
        epoch: u64,
        window_id: u64,
    },
    /// Normal or aborted completion releases that epoch's source barrier.
    /// Clear `active_crossfade` only if its epoch also matches.
    CrossfadeComplete {
        epoch: u64,
    },
    HideSnapHint,
    FocusFollowsMouse {
        window_id: u64,
    },
    /// Debounced display change — fires after WM_DISPLAYCHANGE settles.
    DisplayChangeSettled,
    PowerStateChanged {
        on_battery_or_saver: bool,
    },
    SuspendResume(leopardwm_platform_win32::SuspendResumeEvent),
    UpdateAvailable(String),
    /// User invoked an action on a tab in the tab strip overlay. The
    /// strip captures the column identity (`monitor`, `workspace_idx`,
    /// `row_idx`, `column_idx`) at `show()` time so the action routes to the
    /// column the user *saw* — not whatever happens to be focused when the main
    /// loop processes this event. (Focus can change between the
    /// originating WM_* arriving in the strip's WndProc and the main loop
    /// draining the channel.) `action` discriminates left-click vs.
    /// close/untab/rename surfaces.
    TabAction {
        monitor: isize,
        workspace_idx: usize,
        row_idx: usize,
        column_idx: usize,
        tab_idx: usize,
        action: TabAction,
    },
    /// Low-frequency tick that triggers a tab-strip refresh so background
    /// icon changes (notification badges, app icon swaps that don't
    /// accompany a title change) propagate without user interaction.
    /// Windows has no MSAA event for icon-only changes — `WM_SETICON` is
    /// a per-window message, not a global hook — so polling is the
    /// pragmatic alternative.
    TabStripIconPoll,
    /// Tick that prunes tracked focus which vanished without a WinEvent and
    /// re-checks off-screen moves dropped during placement suppression.
    PeriodicCheck,
    Overview(OverviewEvent),
    /// Rename-dialog result delivered from the spawned modal thread.
    /// `new_title: None` means cancel; `Some("")` means clear the
    /// override and fall back to the live window title; `Some(name)`
    /// installs `name` as the override.
    TabRenameSubmitted {
        monitor: isize,
        workspace_idx: usize,
        row_idx: usize,
        column_idx: usize,
        tab_idx: usize,
        /// HWND captured when the popup was spawned. Authoritative
        /// rename target — independent of the (monitor/ws/row/col/tab)
        /// indices in case the column mutated while the popup was open.
        target_hwnd: u64,
        new_title: Option<String>,
    },
    /// Retry a layout apply that toggle-ignore deferred until apply and
    /// animation workers are idle. Armed by `finish_daemon_event`; the
    /// handler re-arms while consume still reports `Waiting`.
    IdleLayoutReapply,
    /// Retry a display-change placement after the monitor topology settles.
    DisplayChangeApplyRetry(u64),
    /// Debounced persist trigger. Emitted by the background save task
    /// after a quiet period following one or more persisted-state
    /// changes. Handled on the main loop, which builds the snapshot JSON
    /// under its existing `AppState` lock and writes it off-loop. Routed
    /// through the event loop (rather than locking `AppState` directly in
    /// the spawned task) because `AppState` is not `Send`.
    PersistStateNow,
    Shutdown,
}

/// Payload sent from the dialog thread back into the main loop via the
/// mpsc-to-tokio forwarder. Mirrors the strip's click-event pattern.
///
/// `target_hwnd` is captured at spawn time so the rename override lands
/// on the *original* window even if the column mutates (tab closed,
/// reordered, untabbed) while the popup is open. The monitor/ws/row/col/tab
/// indices are kept for diagnostics + strip refresh routing.
#[derive(Debug, Clone)]
pub(crate) struct TabRenameResult {
    pub(crate) monitor: isize,
    pub(crate) workspace_idx: usize,
    pub(crate) row_idx: usize,
    pub(crate) column_idx: usize,
    pub(crate) tab_idx: usize,
    pub(crate) target_hwnd: u64,
    pub(crate) new_title: Option<String>,
}
