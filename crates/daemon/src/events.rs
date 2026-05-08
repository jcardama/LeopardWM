use leopardwm_platform_win32::{GestureEvent, HotkeyEvent, WindowEvent};
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
    /// Receiver attached to the daemon's event broadcaster — guaranteed
    /// to deliver every event sent after the snapshot was taken.
    pub(crate) receiver: broadcast::Receiver<IpcEvent>,
}

/// Events that the daemon event loop processes.
pub(crate) enum DaemonEvent {
    /// An IPC command from a CLI client.
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
    /// A window lifecycle event from Win32.
    WindowEvent(WindowEvent),
    /// A global hotkey was pressed.
    Hotkey(HotkeyEvent),
    /// A touchpad gesture was detected.
    Gesture(GestureEvent),
    /// A tray menu event.
    Tray(tray::TrayEvent),
    /// A settings window event.
    Settings(settings::SettingsEvent),
    /// An animation frame was applied by the worker thread.
    AnimationFrameApplied(animation_worker::FrameResult),
    /// Hide snap hint overlay after timeout.
    HideSnapHint,
    /// Apply focus-follows-mouse focus after delay.
    FocusFollowsMouse { window_id: u64 },
    /// Debounced display change — fires after WM_DISPLAYCHANGE settles.
    DisplayChangeSettled,
    /// Power state changed (AC/battery or power saver toggled).
    PowerStateChanged { on_battery_or_saver: bool },
    /// Update checker observed a newer release tag (e.g. `v0.1.11`).
    UpdateAvailable(String),
    /// Shutdown signal.
    Shutdown,
}
