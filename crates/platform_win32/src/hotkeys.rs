//! System-event window: forwards display-configuration, work-area, and power
//! notifications to the daemon. Hotkey matching lives in `keyboard_hook.rs`.
//!
//! This module also owns the shared hotkey vocabulary (`Modifiers`, `Hotkey`,
//! `HotkeyEvent`, `parse_hotkey_string`) used by the keyboard hook and daemon.

use crate::{recover_poisoned_mutex, Win32Error, WindowEvent};
use std::ffi::c_void;
use std::sync::mpsc;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW,
    PostThreadMessageW, RegisterClassW, UnregisterClassW, MSG, WM_USER, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

/// Window message for display configuration changes.
const WM_DISPLAYCHANGE: u32 = 0x007E;

/// Window message for system-wide setting changes (work area, theme, etc.).
const WM_SETTINGCHANGE: u32 = 0x001A;

/// `SystemParametersInfo` action signalling the desktop work area changed
/// (wparam of `WM_SETTINGCHANGE`). Fired when the taskbar toggles between
/// auto-hide and always-on, which resizes `rcWork` without a display change.
const SPI_SETWORKAREA: usize = 0x002F;

/// Window message for power state changes.
const WM_POWERBROADCAST: u32 = 0x0218;

/// Power setting change notification (wparam for WM_POWERBROADCAST).
const PBT_POWERSETTINGCHANGE: usize = 0x8013;

/// Unique identifier for a registered hotkey.
pub type HotkeyId = i32;

/// Keyboard modifiers for hotkeys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    /// F13–F24 held as modifiers. Bit `i` (0..=11) is F13+`i` (vk 0x7C..=0x87).
    pub fn_mods: u16,
}

/// Map a virtual key in the F13–F24 range (0x7C..=0x87) to its [`Modifiers::fn_mods`]
/// bit, or `None` for any other key. F1–F12 are deliberately excluded: they are
/// common application shortcuts and unsafe to repurpose as modifiers.
pub fn fn_mod_bit(vk: u32) -> Option<u16> {
    (0x7C..=0x87).contains(&vk).then(|| 1u16 << (vk - 0x7C))
}

impl Modifiers {
    /// Create modifiers with only the Win key.
    pub fn win() -> Self {
        Self {
            win: true,
            ..Default::default()
        }
    }

    /// Create modifiers with Win + Shift.
    pub fn win_shift() -> Self {
        Self {
            win: true,
            shift: true,
            ..Default::default()
        }
    }

    /// Create modifiers with Alt.
    pub fn alt() -> Self {
        Self {
            alt: true,
            ..Default::default()
        }
    }

    /// Pack the modifier flags into a bitfield. Bits 0–3 are Ctrl/Alt/Shift/Win
    /// (0–15, unchanged); bits 4–15 carry the F13–F24 modifier mask. Used to
    /// derive stable hotkey IDs that don't depend on config iteration order.
    pub fn bits(&self) -> i32 {
        (self.ctrl as i32)
            | (self.alt as i32) << 1
            | (self.shift as i32) << 2
            | (self.win as i32) << 3
            | (self.fn_mods as i32) << 4
    }
}

/// A hotkey definition with modifiers and virtual key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    /// The unique ID for this hotkey.
    pub id: HotkeyId,
    /// Modifier keys (Ctrl, Alt, Shift, Win).
    pub modifiers: Modifiers,
    /// Virtual key code (e.g., 'H' = 0x48).
    pub vk: u32,
}

impl Hotkey {
    /// Create a new hotkey definition.
    pub fn new(id: HotkeyId, modifiers: Modifiers, vk: u32) -> Self {
        Self { id, modifiers, vk }
    }

    /// Stable ID intrinsic to the `(modifiers, vk)` combo, independent of
    /// config iteration order. Packs the modifier bits above the 8-bit vk:
    /// `(mods << 8) | vk`. The `vk` (bits 0–7) and `mods` (bits 8+) partitions
    /// are disjoint, so IDs stay unique per combo. Standard-modifier IDs are
    /// unchanged; combos using F13–F24 as modifiers exceed the legacy
    /// 0x0000–0xBFFF range, which is now harmless: since the move to a
    /// keyboard hook (away from `RegisterHotKey`) these IDs are only internal
    /// `HashMap` keys, never Win32 atoms, and are never serialized.
    ///
    /// Why: IDs were previously assigned sequentially while iterating a
    /// `HashMap`, so a config reload could remap an ID to a different
    /// command. A `WM_HOTKEY` message already queued under the old ID then
    /// executed the new command — e.g. an innocent keypress firing
    /// `panic_revert`. An intrinsic ID always denotes the same physical
    /// key, so a stale message can only ever run that key's current action.
    pub fn stable_id(modifiers: Modifiers, vk: u32) -> HotkeyId {
        (modifiers.bits() << 8) | (vk as i32 & 0xFF)
    }
}

/// Event emitted when a hotkey is matched by the keyboard hook.
#[derive(Debug, Clone, Copy)]
pub struct HotkeyEvent {
    /// The ID of the hotkey that was pressed.
    pub id: HotkeyId,
}

/// Global sender for display change events forwarded to the window event
/// channel. Registered once at startup (`set_display_change_sender`) and
/// deliberately NOT cleared when the system-event window is dropped: the window
/// is recreated on every config reload, but the daemon's forwarding channel
/// lives for the whole session. Clearing it on window drop silently stopped
/// display-change (and power) events from reaching the daemon after the first
/// reload, so resolution/work-area changes no longer reconciled.
static DISPLAY_CHANGE_SENDER: std::sync::Mutex<Option<mpsc::Sender<WindowEvent>>> =
    std::sync::Mutex::new(None);

/// Global sender for power state change events. Same lifetime as
/// [`DISPLAY_CHANGE_SENDER`]: set once at startup, survives window recreation.
static POWER_STATE_SENDER: std::sync::Mutex<Option<mpsc::Sender<bool>>> =
    std::sync::Mutex::new(None);

/// Custom message to signal the system-event thread to stop.
const WM_QUIT_SYSEVENT_THREAD: u32 = WM_USER + 1;

fn request_sysevent_thread_shutdown(hwnd: HWND, thread_id: u32) -> bool {
    let mut shutdown_signal_sent = unsafe {
        PostMessageW(
            Some(hwnd),
            WM_QUIT_SYSEVENT_THREAD,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        )
        .is_ok()
    };

    if !shutdown_signal_sent {
        tracing::warn!(
            "PostMessageW quit signal failed for system-event window {:?}; attempting thread message fallback",
            hwnd
        );
        shutdown_signal_sent = unsafe {
            PostThreadMessageW(
                thread_id,
                WM_QUIT_SYSEVENT_THREAD,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            )
            .is_ok()
        };
        if !shutdown_signal_sent {
            tracing::warn!(
                "PostThreadMessageW quit signal failed for system-event thread {}; proceeding without blocking join",
                thread_id
            );
        }
    }

    shutdown_signal_sent
}

/// Handle for the system-event message window and thread.
///
/// Dropping this handle stops the message loop and tears the window down.
pub struct SystemEventHandle {
    hwnd: HWND,
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SystemEventHandle {
    fn drop(&mut self) {
        // Signal the message loop to quit
        let shutdown_signal_sent = request_sysevent_thread_shutdown(self.hwnd, self.thread_id);

        // Join if finished; otherwise detach to avoid hanging shutdown.
        if let Some(thread) = self.thread.take() {
            if shutdown_signal_sent {
                const HOTKEY_THREAD_WAIT_POLLS: usize = 30;
                const HOTKEY_THREAD_WAIT_MS: u64 = 10;
                for _ in 0..HOTKEY_THREAD_WAIT_POLLS {
                    if thread.is_finished() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(HOTKEY_THREAD_WAIT_MS));
                }
            }

            if thread.is_finished() {
                let _ = thread.join();
            } else {
                tracing::warn!(
                    "System-event thread did not exit promptly after shutdown signal; detaching to avoid hang"
                );
            }
        }

        // The display/power forwarding senders are intentionally left intact:
        // they belong to the daemon for the whole session, and this window is
        // recreated on every config reload. Clearing them here would stop
        // display-change and power events from reconciling after one reload.
    }
}

/// Register a sender for display change events.
///
/// This allows the hotkey window to forward WM_DISPLAYCHANGE messages
/// to the window event channel. Call this before `register_system_events`.
pub fn set_display_change_sender(sender: mpsc::Sender<WindowEvent>) -> Result<(), Win32Error> {
    let mut guard = DISPLAY_CHANGE_SENDER.lock().map_err(|_| {
        Win32Error::HookInstallFailed("Display change sender mutex poisoned".to_string())
    })?;
    *guard = Some(sender);
    Ok(())
}

/// Register a sender for power state change events.
///
/// This allows the hotkey window to forward `WM_POWERBROADCAST` notifications
/// to the daemon event loop. Call this before `register_system_events`.
pub fn set_power_state_sender(sender: mpsc::Sender<bool>) -> Result<(), Win32Error> {
    let mut guard = POWER_STATE_SENDER.lock().map_err(|_| {
        Win32Error::HookInstallFailed("Power state sender mutex poisoned".to_string())
    })?;
    *guard = Some(sender);
    Ok(())
}

/// Create the hidden system-event window and start its message loop.
///
/// The window receives `WM_DISPLAYCHANGE`, `WM_SETTINGCHANGE` (work area), and
/// `WM_POWERBROADCAST` and forwards them through the senders registered with
/// [`set_display_change_sender`] / [`set_power_state_sender`]. Hotkeys are
/// matched separately by the keyboard hook (`keyboard_hook.rs`).
///
/// # Returns
/// * Handle to keep the window alive (drop to tear it down)
pub fn register_system_events() -> Result<SystemEventHandle, Win32Error> {
    // Create the message window on a separate thread.
    // We send isize (raw pointer value) instead of HWND because HWND is !Send
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(isize, u32), Win32Error>>();

    let thread = std::thread::spawn(move || {
        unsafe {
            let thread_id = GetCurrentThreadId();

            // Register window class
            let class_name: Vec<u16> = "LeopardWMSysEventClass\0".encode_utf16().collect();
            let wc = WNDCLASSW {
                lpfnWndProc: Some(sysevent_window_proc),
                lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            RegisterClassW(&wc);

            // Create a hidden top-level window.
            // WM_DISPLAYCHANGE is broadcast to top-level windows, but not to message-only windows.
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                windows::core::PCWSTR(class_name.as_ptr()),
                None,
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            );

            if hwnd.is_err() {
                let _ = init_tx.send(Err(Win32Error::HotkeyRegistrationFailed(
                    "Failed to create message window".to_string(),
                )));
                return;
            }

            let hwnd = hwnd.unwrap();

            // Register for power state notifications on this window
            {
                use windows::Win32::System::Power::RegisterPowerSettingNotification;
                use windows::Win32::UI::WindowsAndMessaging::REGISTER_NOTIFICATION_FLAGS;

                // GUID_ACDC_POWER_SOURCE — fires on AC/battery/UPS transitions
                const GUID_ACDC_POWER_SOURCE: windows::core::GUID =
                    windows::core::GUID::from_u128(0x5d3e9a59_e9d5_4b00_a6bd_ff34ff516548);
                // GUID_POWER_SAVING_STATUS — fires when power saver toggles
                const GUID_POWER_SAVING_STATUS: windows::core::GUID =
                    windows::core::GUID::from_u128(0xe00958c0_c213_4ace_ac77_fecced2eeea5);

                let handle = windows::Win32::Foundation::HANDLE(hwnd.0);
                if let Err(e) = RegisterPowerSettingNotification(
                    handle,
                    &GUID_ACDC_POWER_SOURCE,
                    REGISTER_NOTIFICATION_FLAGS(0),
                ) {
                    tracing::warn!("Failed to register GUID_ACDC_POWER_SOURCE notification: {}", e);
                }
                if let Err(e) = RegisterPowerSettingNotification(
                    handle,
                    &GUID_POWER_SAVING_STATUS,
                    REGISTER_NOTIFICATION_FLAGS(0),
                ) {
                    tracing::warn!("Failed to register GUID_POWER_SAVING_STATUS notification: {}", e);
                }
                tracing::debug!("Registered power setting notifications");
            }

            // Send initialization result (hwnd as isize for Send safety)
            let hwnd_raw = hwnd.0 as isize;
            let _ = init_tx.send(Ok((hwnd_raw, thread_id)));

            // Message loop
            let mut msg = MSG::default();
            loop {
                let get_message_result = GetMessageW(&mut msg, None, 0, 0).0;
                if get_message_result <= 0 {
                    break;
                }
                if msg.message == WM_QUIT_SYSEVENT_THREAD {
                    break;
                }
                let _ = DispatchMessageW(&msg);
            }

            let _ = DestroyWindow(hwnd);
            let _ = UnregisterClassW(windows::core::PCWSTR(class_name.as_ptr()), None);
        }
    });

    // Wait for initialization
    let init_result = init_rx.recv().map_err(|_| {
        Win32Error::HotkeyRegistrationFailed("Thread initialization failed".to_string())
    })?;
    let (hwnd_raw, thread_id) = match init_result {
        Ok(v) => v,
        Err(e) => {
            if thread.is_finished() {
                let _ = thread.join();
            }
            return Err(e);
        }
    };

    // Reconstruct HWND from raw pointer
    let hwnd = HWND(hwnd_raw as *mut c_void);

    Ok(SystemEventHandle {
        hwnd,
        thread_id,
        thread: Some(thread),
    })
}

/// Window procedure for the system-event window.
///
/// Wrapped with catch_unwind to prevent panics from crashing the application.
unsafe extern "system" fn sysevent_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    // Wrap in catch_unwind to prevent panics from crashing
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sysevent_window_proc_inner(hwnd, msg, wparam, lparam)
    }));

    match result {
        Ok(lresult) => lresult,
        Err(e) => {
            tracing::error!("Panic in sysevent_window_proc: {:?}", e);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

/// Inner implementation of the system-event window procedure.
fn sysevent_window_proc_inner(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    match msg {
        WM_DISPLAYCHANGE => {
            tracing::info!("Display configuration changed (WM_DISPLAYCHANGE)");

            // Send display change event through window event channel (recover from mutex poisoning)
            let sender_guard = DISPLAY_CHANGE_SENDER
                .lock()
                .unwrap_or_else(recover_poisoned_mutex);
            if let Some(sender) = sender_guard.as_ref() {
                let _ = sender.send(WindowEvent::DisplayChange);
            }

            windows::Win32::Foundation::LRESULT(0)
        }
        WM_SETTINGCHANGE if wparam.0 == SPI_SETWORKAREA => {
            // The work area changed without a topology change (e.g. the
            // taskbar toggled between auto-hide and always-on). Route it
            // through a shorter-debounced reconcile than a display change so
            // tiled windows re-fit promptly (the OS shoves them flush against
            // the new taskbar immediately; a slow reconcile makes that a
            // visible two-step).
            let sender_guard = DISPLAY_CHANGE_SENDER
                .lock()
                .unwrap_or_else(recover_poisoned_mutex);
            if let Some(sender) = sender_guard.as_ref() {
                let _ = sender.send(WindowEvent::WorkAreaChanged);
            }
            windows::Win32::Foundation::LRESULT(0)
        }
        WM_POWERBROADCAST => {
            if wparam.0 == PBT_POWERSETTINGCHANGE {
                let on_battery_or_saver = crate::system::is_on_battery_or_power_saver();
                tracing::debug!("Power state changed: on_battery_or_saver={}", on_battery_or_saver);

                let sender_guard = POWER_STATE_SENDER
                    .lock()
                    .unwrap_or_else(recover_poisoned_mutex);
                if let Some(sender) = sender_guard.as_ref() {
                    let _ = sender.send(on_battery_or_saver);
                }

                windows::Win32::Foundation::LRESULT(1) // TRUE = processed
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Common virtual key codes for hotkey registration.
pub mod vk {
    // Letters
    pub const A: u32 = 0x41;
    pub const B: u32 = 0x42;
    pub const C: u32 = 0x43;
    pub const D: u32 = 0x44;
    pub const E: u32 = 0x45;
    pub const F: u32 = 0x46;
    pub const G: u32 = 0x47;
    pub const H: u32 = 0x48;
    pub const I: u32 = 0x49;
    pub const J: u32 = 0x4A;
    pub const K: u32 = 0x4B;
    pub const L: u32 = 0x4C;
    pub const M: u32 = 0x4D;
    pub const N: u32 = 0x4E;
    pub const O: u32 = 0x4F;
    pub const P: u32 = 0x50;
    pub const Q: u32 = 0x51;
    pub const R: u32 = 0x52;
    pub const S: u32 = 0x53;
    pub const T: u32 = 0x54;
    pub const U: u32 = 0x55;
    pub const V: u32 = 0x56;
    pub const W: u32 = 0x57;
    pub const X: u32 = 0x58;
    pub const Y: u32 = 0x59;
    pub const Z: u32 = 0x5A;

    // Numbers
    pub const N0: u32 = 0x30;
    pub const N1: u32 = 0x31;
    pub const N2: u32 = 0x32;
    pub const N3: u32 = 0x33;
    pub const N4: u32 = 0x34;
    pub const N5: u32 = 0x35;
    pub const N6: u32 = 0x36;
    pub const N7: u32 = 0x37;
    pub const N8: u32 = 0x38;
    pub const N9: u32 = 0x39;

    // Function keys
    pub const F1: u32 = 0x70;
    pub const F2: u32 = 0x71;
    pub const F3: u32 = 0x72;
    pub const F4: u32 = 0x73;
    pub const F5: u32 = 0x74;
    pub const F6: u32 = 0x75;
    pub const F7: u32 = 0x76;
    pub const F8: u32 = 0x77;
    pub const F9: u32 = 0x78;
    pub const F10: u32 = 0x79;
    pub const F11: u32 = 0x7A;
    pub const F12: u32 = 0x7B;

    // Navigation
    pub const LEFT: u32 = 0x25;
    pub const UP: u32 = 0x26;
    pub const RIGHT: u32 = 0x27;
    pub const DOWN: u32 = 0x28;
    pub const HOME: u32 = 0x24;
    pub const END: u32 = 0x23;

    // Other
    pub const TAB: u32 = 0x09;
    pub const SPACE: u32 = 0x20;
    pub const ENTER: u32 = 0x0D;
    pub const ESCAPE: u32 = 0x1B;

    // Punctuation (for common shortcuts)
    pub const MINUS: u32 = 0xBD; // '-'
    pub const EQUALS: u32 = 0xBB; // '='
    pub const BRACKET_LEFT: u32 = 0xDB; // '['
    pub const BRACKET_RIGHT: u32 = 0xDD; // ']'
    pub const COMMA: u32 = 0xBC; // ','
    pub const PERIOD: u32 = 0xBE; // '.'
}

/// Parse a virtual key code from a key name string.
///
/// Supports single letters (A-Z), numbers (0-9), function keys (F1-F12),
/// and special keys (Left, Right, Up, Down, Tab, Space, Enter, Escape).
pub fn parse_vk(key: &str) -> Option<u32> {
    let key = key.trim().to_uppercase();

    // Single letter
    if key.len() == 1 {
        let c = key.chars().next()?;
        if c.is_ascii_uppercase() {
            return Some(c as u32);
        }
        if c.is_ascii_digit() {
            return Some(c as u32);
        }
    }

    // Function keys
    if key.starts_with('F') && key.len() <= 3 {
        if let Ok(n) = key[1..].parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(0x6F + n); // F1=0x70 .. F24=0x87
            }
        }
    }

    // Named keys
    match key.as_str() {
        "LEFT" => Some(vk::LEFT),
        "RIGHT" => Some(vk::RIGHT),
        "UP" => Some(vk::UP),
        "DOWN" => Some(vk::DOWN),
        "HOME" => Some(vk::HOME),
        "END" => Some(vk::END),
        "TAB" => Some(vk::TAB),
        "SPACE" => Some(vk::SPACE),
        "ENTER" | "RETURN" => Some(vk::ENTER),
        "ESCAPE" | "ESC" => Some(vk::ESCAPE),
        "MINUS" | "-" => Some(vk::MINUS),
        "EQUALS" | "PLUS" | "=" => Some(vk::EQUALS),
        "COMMA" | "," => Some(vk::COMMA),
        "PERIOD" | "." => Some(vk::PERIOD),
        "BRACKET_LEFT" | "[" => Some(vk::BRACKET_LEFT),
        "BRACKET_RIGHT" | "]" => Some(vk::BRACKET_RIGHT),
        _ => None,
    }
}

/// Parse a hotkey string like "Win+Shift+H" into modifiers and virtual key code.
///
/// Modifier tokens are Ctrl/Alt/Shift/Win plus F13–F24 (e.g. "F13+H"); F1–F12
/// are keys only, never modifiers. The final token is always the key.
///
/// Returns modifiers and virtual key code if valid.
pub fn parse_hotkey_string(s: &str) -> Option<(Modifiers, u32)> {
    let parts: Vec<&str> = s.split('+').map(|p| p.trim()).collect();
    if parts.is_empty() {
        return None;
    }

    let mut modifiers = Modifiers::default();

    // Last part is the key, rest are modifiers
    for part in &parts[..parts.len() - 1] {
        match part.to_uppercase().as_str() {
            "CTRL" | "CONTROL" => modifiers.ctrl = true,
            "ALT" => modifiers.alt = true,
            "SHIFT" => modifiers.shift = true,
            "WIN" | "SUPER" | "META" => modifiers.win = true,
            // F13–F24 may act as modifiers; F1–F12 may not.
            other => match parse_vk(other).and_then(fn_mod_bit) {
                Some(bit) => modifiers.fn_mods |= bit,
                None => return None, // Unknown modifier
            },
        }
    }

    // Parse the key
    let key = parts.last()?;
    let vk = parse_vk(key)?;

    Some((modifiers, vk))
}

#[cfg(test)]
mod tests {
    use super::*;

    static HOTKEY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_stable_id_is_deterministic_and_unique_per_combo() {
        let ctrl_alt = Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        // Same combo -> same id across calls (reload stability).
        assert_eq!(
            Hotkey::stable_id(ctrl_alt, 0xBC),
            Hotkey::stable_id(ctrl_alt, 0xBC)
        );
        // Different vk, same mods -> different id.
        assert_ne!(
            Hotkey::stable_id(ctrl_alt, 0xBC),
            Hotkey::stable_id(ctrl_alt, 0xBE)
        );
        // Same vk, different mods -> different id.
        let win = Modifiers::win();
        assert_ne!(
            Hotkey::stable_id(ctrl_alt, 0x1B),
            Hotkey::stable_id(win, 0x1B)
        );
        // Standard-modifier ids stay within the legacy app hotkey range
        // (0x0000-0xBFFF).
        let all_mods = Modifiers {
            ctrl: true,
            alt: true,
            shift: true,
            win: true,
            ..Default::default()
        };
        assert!(Hotkey::stable_id(all_mods, 0xFF) <= 0xBFFF);
        assert!(Hotkey::stable_id(all_mods, 0xFF) > 0);

        // F13–F24 modifiers occupy a disjoint bit range, so they stay unique
        // per combo (and may exceed the legacy range, which is now fine).
        let f13 = Modifiers {
            fn_mods: fn_mod_bit(0x7C).unwrap(),
            ..Default::default()
        };
        let f14 = Modifiers {
            fn_mods: fn_mod_bit(0x7D).unwrap(),
            ..Default::default()
        };
        assert_ne!(Hotkey::stable_id(f13, 0x48), Hotkey::stable_id(f14, 0x48));
        assert_ne!(Hotkey::stable_id(f13, 0x48), Hotkey::stable_id(ctrl_alt, 0x48));
        // F13+H and a bare standard combo can't collide: same vk, different mods.
        assert_ne!(Hotkey::stable_id(f13, 0x48), Hotkey::stable_id(win, 0x48));
    }

    #[test]
    fn test_sysevent_handle_drop_preserves_forwarding_senders() {
        let _guard = HOTKEY_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);

        let (display_tx, _display_rx) = mpsc::channel::<WindowEvent>();
        let (power_tx, _power_rx) = mpsc::channel::<bool>();
        {
            let mut g = DISPLAY_CHANGE_SENDER
                .lock()
                .unwrap_or_else(recover_poisoned_mutex);
            *g = Some(display_tx);
        }
        {
            let mut g = POWER_STATE_SENDER
                .lock()
                .unwrap_or_else(recover_poisoned_mutex);
            *g = Some(power_tx);
        }

        // Dropping the system-event window happens on every config reload. It
        // must NOT clear the daemon's forwarding senders, or display-change and
        // power events stop reaching the daemon after the first reload.
        let finished = std::thread::spawn(|| {});
        for _ in 0..100 {
            if finished.is_finished() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        drop(SystemEventHandle {
            hwnd: HWND(std::ptr::null_mut()),
            thread_id: 0,
            thread: Some(finished),
        });

        let display_sender = DISPLAY_CHANGE_SENDER
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        assert!(
            display_sender.is_some(),
            "display sender must survive window drop"
        );
        drop(display_sender);
        let power_sender = POWER_STATE_SENDER
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        assert!(
            power_sender.is_some(),
            "power sender must survive window drop"
        );
        drop(power_sender);

        // Cleanup for other tests.
        *DISPLAY_CHANGE_SENDER
            .lock()
            .unwrap_or_else(recover_poisoned_mutex) = None;
        *POWER_STATE_SENDER
            .lock()
            .unwrap_or_else(recover_poisoned_mutex) = None;
    }

    #[test]
    fn test_sysevent_window_proc_forwards_display_change_events() {
        let _guard = HOTKEY_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);

        let (tx, rx) = mpsc::channel::<WindowEvent>();
        set_display_change_sender(tx).unwrap();

        let lresult = sysevent_window_proc_inner(
            HWND(std::ptr::null_mut()),
            WM_DISPLAYCHANGE,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        );
        assert_eq!(lresult.0, 0);

        let event = rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .expect("display change event should be forwarded");
        assert!(matches!(event, WindowEvent::DisplayChange));

        let mut sender_guard = DISPLAY_CHANGE_SENDER
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        *sender_guard = None;
    }

    #[test]
    fn test_sysevent_handle_drop_does_not_block_when_quit_post_fails() {
        let _guard = HOTKEY_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);

        let sleeping_thread = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(1));
        });

        let start = std::time::Instant::now();
        drop(SystemEventHandle {
            hwnd: HWND(std::ptr::null_mut()),
            thread_id: 0,
            thread: Some(sleeping_thread),
        });
        assert!(start.elapsed() < std::time::Duration::from_millis(500));
    }

    #[test]
    fn test_parse_vk() {
        // Letters
        assert_eq!(parse_vk("H"), Some(vk::H));
        assert_eq!(parse_vk("h"), Some(vk::H));
        assert_eq!(parse_vk("L"), Some(vk::L));

        // Numbers
        assert_eq!(parse_vk("1"), Some(vk::N1));
        assert_eq!(parse_vk("0"), Some(vk::N0));

        // Function keys
        assert_eq!(parse_vk("F1"), Some(vk::F1));
        assert_eq!(parse_vk("F12"), Some(vk::F12));
        assert_eq!(parse_vk("f5"), Some(vk::F5));
        assert_eq!(parse_vk("F13"), Some(0x7C));
        assert_eq!(parse_vk("F24"), Some(0x87));

        // Navigation
        assert_eq!(parse_vk("Left"), Some(vk::LEFT));
        assert_eq!(parse_vk("RIGHT"), Some(vk::RIGHT));
        assert_eq!(parse_vk("Home"), Some(vk::HOME));
        assert_eq!(parse_vk("end"), Some(vk::END));

        // Special keys
        assert_eq!(parse_vk("Tab"), Some(vk::TAB));
        assert_eq!(parse_vk("Space"), Some(vk::SPACE));
        assert_eq!(parse_vk("Enter"), Some(vk::ENTER));
        assert_eq!(parse_vk("Escape"), Some(vk::ESCAPE));

        // Invalid
        assert_eq!(parse_vk("Invalid"), None);
        assert_eq!(parse_vk("F25"), None);
    }

    #[test]
    fn test_parse_hotkey_string() {
        // Win+H
        let (mods, vk) = parse_hotkey_string("Win+H").unwrap();
        assert!(mods.win);
        assert!(!mods.ctrl);
        assert!(!mods.alt);
        assert!(!mods.shift);
        assert_eq!(vk, super::vk::H);

        // Ctrl+Alt+Left
        let (mods, vk) = parse_hotkey_string("Ctrl+Alt+Left").unwrap();
        assert!(mods.ctrl);
        assert!(mods.alt);
        assert!(!mods.win);
        assert_eq!(vk, super::vk::LEFT);

        // Win+Shift+L
        let (mods, vk) = parse_hotkey_string("Win+Shift+L").unwrap();
        assert!(mods.win);
        assert!(mods.shift);
        assert_eq!(vk, super::vk::L);

        // Case insensitive
        let (mods, _) = parse_hotkey_string("win+shift+h").unwrap();
        assert!(mods.win);
        assert!(mods.shift);

        // Invalid modifier
        assert!(parse_hotkey_string("Foo+H").is_none());

        // Invalid key
        assert!(parse_hotkey_string("Win+InvalidKey").is_none());

        // F13–F24 as modifiers.
        let (mods, vk) = parse_hotkey_string("F13+H").unwrap();
        assert_eq!(mods.fn_mods, fn_mod_bit(0x7C).unwrap());
        assert!(!mods.ctrl && !mods.alt && !mods.shift && !mods.win);
        assert_eq!(vk, super::vk::H);

        // Multiple F-key modifiers combine.
        let (mods, vk) = parse_hotkey_string("F13+F14+H").unwrap();
        assert_eq!(mods.fn_mods, fn_mod_bit(0x7C).unwrap() | fn_mod_bit(0x7D).unwrap());
        assert_eq!(vk, super::vk::H);

        // F-key modifier mixes with standard modifiers.
        let (mods, _) = parse_hotkey_string("Ctrl+F13+H").unwrap();
        assert!(mods.ctrl);
        assert_eq!(mods.fn_mods, fn_mod_bit(0x7C).unwrap());

        // F1–F12 are not valid modifiers.
        assert!(parse_hotkey_string("F12+H").is_none());

        // A bare F13 is a trigger, not a modifier.
        let (mods, vk) = parse_hotkey_string("F13").unwrap();
        assert_eq!(mods.fn_mods, 0);
        assert_eq!(vk, 0x7C);
    }
}
