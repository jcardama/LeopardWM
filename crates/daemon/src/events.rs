use leopardwm_platform_win32::{GestureEvent, HotkeyEvent, WindowEvent};
use tokio::sync::oneshot;

use crate::animation_worker;
use crate::settings;
use crate::tray;
use leopardwm_ipc::{IpcCommand, IpcResponse};

/// Events that the daemon event loop processes.
pub(crate) enum DaemonEvent {
    /// An IPC command from a CLI client.
    IpcCommand {
        cmd: IpcCommand,
        responder: oneshot::Sender<IpcResponse>,
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
    /// Shutdown signal.
    Shutdown,
}
