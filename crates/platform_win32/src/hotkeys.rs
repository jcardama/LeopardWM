//! Global hotkey registration and event forwarding.

use crate::{recover_poisoned_mutex, Win32Error, WindowEvent};
use std::ffi::c_void;
use std::sync::mpsc;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostMessageW,
    PostThreadMessageW, RegisterClassW, UnregisterClassW, MSG, WM_HOTKEY, WM_USER, WNDCLASSW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

/// Window message for display configuration changes.
const WM_DISPLAYCHANGE: u32 = 0x007E;

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

    /// Convert to Win32 HOT_KEY_MODIFIERS flags.
    pub fn to_win32(&self) -> HOT_KEY_MODIFIERS {
        let mut mods = MOD_NOREPEAT; // Prevent key repeat
        if self.ctrl {
            mods |= MOD_CONTROL;
        }
        if self.alt {
            mods |= MOD_ALT;
        }
        if self.shift {
            mods |= MOD_SHIFT;
        }
        if self.win {
            mods |= MOD_WIN;
        }
        mods
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
}

/// Event emitted when a hotkey is pressed.
#[derive(Debug, Clone, Copy)]
pub struct HotkeyEvent {
    /// The ID of the hotkey that was pressed.
    pub id: HotkeyId,
}

/// Global sender for hotkey events.
/// Uses Mutex to allow re-registration after dropping previous HotkeyHandle.
static HOTKEY_SENDER: std::sync::Mutex<Option<mpsc::Sender<HotkeyEvent>>> =
    std::sync::Mutex::new(None);

/// Global sender for display change events forwarded to window event channel.
/// Uses Mutex to allow re-registration after dropping previous EventHookHandle.
static DISPLAY_CHANGE_SENDER: std::sync::Mutex<Option<mpsc::Sender<WindowEvent>>> =
    std::sync::Mutex::new(None);

/// Global sender for power state change events.
static POWER_STATE_SENDER: std::sync::Mutex<Option<mpsc::Sender<bool>>> =
    std::sync::Mutex::new(None);

/// Custom message to signal the hotkey thread to stop.
const WM_QUIT_HOTKEY_THREAD: u32 = WM_USER + 1;

fn clear_hotkey_globals() {
    let mut sender = HOTKEY_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
    *sender = None;
    drop(sender);
    let mut sender = DISPLAY_CHANGE_SENDER
        .lock()
        .unwrap_or_else(recover_poisoned_mutex);
    *sender = None;
    drop(sender);
    let mut sender = POWER_STATE_SENDER
        .lock()
        .unwrap_or_else(recover_poisoned_mutex);
    *sender = None;
}

fn request_hotkey_thread_shutdown(hwnd: HWND, thread_id: u32) -> bool {
    let mut shutdown_signal_sent = unsafe {
        PostMessageW(
            Some(hwnd),
            WM_QUIT_HOTKEY_THREAD,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        )
        .is_ok()
    };

    if !shutdown_signal_sent {
        tracing::warn!(
            "PostMessageW quit signal failed for hotkey window {:?}; attempting thread message fallback",
            hwnd
        );
        shutdown_signal_sent = unsafe {
            PostThreadMessageW(
                thread_id,
                WM_QUIT_HOTKEY_THREAD,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            )
            .is_ok()
        };
        if !shutdown_signal_sent {
            tracing::warn!(
                "PostThreadMessageW quit signal failed for hotkey thread {}; proceeding without blocking join",
                thread_id
            );
        }
    }

    shutdown_signal_sent
}

/// Handle for the hotkey message window and thread.
///
/// Dropping this handle will unregister all hotkeys and stop the message loop.
pub struct HotkeyHandle {
    hwnd: HWND,
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
    registered_ids: Vec<HotkeyId>,
}

impl HotkeyHandle {
    /// Returns the number of successfully registered hotkeys.
    pub fn registered_count(&self) -> usize {
        self.registered_ids.len()
    }
}

impl Drop for HotkeyHandle {
    fn drop(&mut self) {
        // Unregister all hotkeys
        unsafe {
            for id in &self.registered_ids {
                let _ = UnregisterHotKey(Some(self.hwnd), *id);
            }
        }
        tracing::debug!("Unregistered {} hotkeys", self.registered_ids.len());

        // Signal the message loop to quit
        let shutdown_signal_sent = request_hotkey_thread_shutdown(self.hwnd, self.thread_id);

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
                    "Hotkey thread did not exit promptly after shutdown signal; detaching to avoid hang"
                );
            }
        }

        // Clear the global senders to allow re-registration (recover from mutex poisoning)
        clear_hotkey_globals();
    }
}

/// Register a sender for display change events.
///
/// This allows the hotkey window to forward WM_DISPLAYCHANGE messages
/// to the window event channel. Call this before `register_hotkeys`.
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
/// to the daemon event loop. Call this before `register_hotkeys`.
pub fn set_power_state_sender(sender: mpsc::Sender<bool>) -> Result<(), Win32Error> {
    let mut guard = POWER_STATE_SENDER.lock().map_err(|_| {
        Win32Error::HookInstallFailed("Power state sender mutex poisoned".to_string())
    })?;
    *guard = Some(sender);
    Ok(())
}

/// Register global hotkeys and start listening for them.
///
/// Returns a handle that must be kept alive to receive hotkey events,
/// and a channel receiver for hotkey events.
///
/// # Arguments
/// * `hotkeys` - List of hotkeys to register
///
/// # Returns
/// * Handle to manage the hotkeys (drop to unregister)
/// * Receiver for hotkey press events
pub fn register_hotkeys(
    hotkeys: Vec<Hotkey>,
) -> Result<(HotkeyHandle, mpsc::Receiver<HotkeyEvent>), Win32Error> {
    // Create channel for events
    let (tx, rx) = mpsc::channel();

    // Store sender globally (check that it's not already set)
    {
        let mut sender = HOTKEY_SENDER.lock().map_err(|_| {
            Win32Error::HotkeyRegistrationFailed("Hotkey sender mutex poisoned".to_string())
        })?;
        if sender.is_some() {
            return Err(Win32Error::HotkeyRegistrationFailed(
                "Hotkey sender already initialized - drop existing HotkeyHandle first".to_string(),
            ));
        }
        *sender = Some(tx);
    }

    // Create the message window and register hotkeys on a separate thread
    // We send isize (raw pointer value) instead of HWND because HWND is !Send
    let (init_tx, init_rx) =
        std::sync::mpsc::channel::<Result<(isize, u32, Vec<HotkeyId>), Win32Error>>();
    let hotkeys_clone = hotkeys.clone();

    let thread = std::thread::spawn(move || {
        unsafe {
            let thread_id = GetCurrentThreadId();

            // Register window class
            let class_name: Vec<u16> = "LeopardWMHotkeyClass\0".encode_utf16().collect();
            let wc = WNDCLASSW {
                lpfnWndProc: Some(hotkey_window_proc),
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
            let mut registered_ids = Vec::new();

            // Register all hotkeys
            for hotkey in &hotkeys_clone {
                let result = RegisterHotKey(
                    Some(hwnd),
                    hotkey.id,
                    hotkey.modifiers.to_win32(),
                    hotkey.vk,
                );

                let mod_str = format!("{}{}{}{}",
                    if hotkey.modifiers.win { "Win+" } else { "" },
                    if hotkey.modifiers.ctrl { "Ctrl+" } else { "" },
                    if hotkey.modifiers.alt { "Alt+" } else { "" },
                    if hotkey.modifiers.shift { "Shift+" } else { "" },
                );
                if result.is_ok() {
                    registered_ids.push(hotkey.id);
                    tracing::debug!("Registered hotkey {} ({}vk=0x{:X})", hotkey.id, mod_str, hotkey.vk);
                } else {
                    let err = std::io::Error::last_os_error();
                    tracing::warn!(
                        "Failed to register hotkey {} ({}vk=0x{:X}) - {}",
                        hotkey.id,
                        mod_str,
                        hotkey.vk,
                        err,
                    );
                }
            }

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
            let _ = init_tx.send(Ok((hwnd_raw, thread_id, registered_ids)));

            // Message loop
            let mut msg = MSG::default();
            loop {
                let get_message_result = GetMessageW(&mut msg, None, 0, 0).0;
                if get_message_result <= 0 {
                    break;
                }
                if msg.message == WM_QUIT_HOTKEY_THREAD {
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
        clear_hotkey_globals();
        Win32Error::HotkeyRegistrationFailed("Thread initialization failed".to_string())
    })?;
    let (hwnd_raw, thread_id, registered_ids) = match init_result {
        Ok(v) => v,
        Err(e) => {
            clear_hotkey_globals();
            if thread.is_finished() {
                let _ = thread.join();
            }
            return Err(e);
        }
    };

    // Reconstruct HWND from raw pointer
    let hwnd = HWND(hwnd_raw as *mut c_void);

    if !hotkeys.is_empty() && registered_ids.len() != hotkeys.len() {
        tracing::warn!(
            "Hotkey registration incomplete ({}/{}); aborting and unregistering all to avoid partial shortcut state",
            registered_ids.len(),
            hotkeys.len()
        );
        let _ = request_hotkey_thread_shutdown(hwnd, thread_id);
        if thread.is_finished() {
            let _ = thread.join();
        }
        clear_hotkey_globals();
        return Err(Win32Error::HotkeyRegistrationFailed(format!(
            "Only {}/{} hotkeys were registered; refusing to run with partial global shortcuts",
            registered_ids.len(),
            hotkeys.len()
        )));
    }
    if !hotkeys.is_empty() {
        tracing::info!(
            "Registered {}/{} hotkeys",
            registered_ids.len(),
            hotkeys.len()
        );
    }

    Ok((
        HotkeyHandle {
            hwnd,
            thread_id,
            thread: Some(thread),
            registered_ids,
        },
        rx,
    ))
}

/// Window procedure for the hotkey message window.
///
/// Wrapped with catch_unwind to prevent panics from crashing the application.
unsafe extern "system" fn hotkey_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    // Wrap in catch_unwind to prevent panics from crashing
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        hotkey_window_proc_inner(hwnd, msg, wparam, lparam)
    }));

    match result {
        Ok(lresult) => lresult,
        Err(e) => {
            tracing::error!("Panic in hotkey_window_proc: {:?}", e);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

/// Inner implementation of hotkey window procedure.
fn hotkey_window_proc_inner(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    match msg {
        WM_HOTKEY => {
            let hotkey_id = wparam.0 as HotkeyId;
            tracing::debug!("Hotkey {} pressed", hotkey_id);

            // Send event through channel (recover from mutex poisoning)
            let sender_guard = HOTKEY_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
            if let Some(sender) = sender_guard.as_ref() {
                let _ = sender.send(HotkeyEvent { id: hotkey_id });
            }

            windows::Win32::Foundation::LRESULT(0)
        }
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
        WM_POWERBROADCAST => {
            if wparam.0 == PBT_POWERSETTINGCHANGE {
                let on_battery_or_saver = crate::utils::is_on_battery_or_power_saver();
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
            if (1..=12).contains(&n) {
                return Some(0x6F + n); // F1=0x70, F2=0x71, ...
            }
        }
    }

    // Named keys
    match key.as_str() {
        "LEFT" => Some(vk::LEFT),
        "RIGHT" => Some(vk::RIGHT),
        "UP" => Some(vk::UP),
        "DOWN" => Some(vk::DOWN),
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
            _ => return None, // Unknown modifier
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
    fn test_clear_hotkey_globals_resets_senders() {
        let _guard = HOTKEY_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);

        let (hotkey_tx, _hotkey_rx) = mpsc::channel::<HotkeyEvent>();
        let (display_tx, _display_rx) = mpsc::channel::<WindowEvent>();

        {
            let mut sender_guard = HOTKEY_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
            *sender_guard = Some(hotkey_tx);
        }
        {
            let mut sender_guard = DISPLAY_CHANGE_SENDER
                .lock()
                .unwrap_or_else(recover_poisoned_mutex);
            *sender_guard = Some(display_tx);
        }

        clear_hotkey_globals();

        let hotkey_sender = HOTKEY_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
        assert!(hotkey_sender.is_none());
        drop(hotkey_sender);
        let display_sender = DISPLAY_CHANGE_SENDER
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);
        assert!(display_sender.is_none());
    }

    #[test]
    fn test_hotkey_window_proc_forwards_hotkey_events() {
        let _guard = HOTKEY_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);

        let (tx, rx) = mpsc::channel::<HotkeyEvent>();
        {
            let mut sender_guard = HOTKEY_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
            *sender_guard = Some(tx);
        }

        let lresult = hotkey_window_proc_inner(
            HWND(std::ptr::null_mut()),
            WM_HOTKEY,
            windows::Win32::Foundation::WPARAM(77),
            windows::Win32::Foundation::LPARAM(0),
        );
        assert_eq!(lresult.0, 0);

        let event = rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .expect("hotkey event should be forwarded");
        assert_eq!(event.id, 77);

        let mut sender_guard = HOTKEY_SENDER.lock().unwrap_or_else(recover_poisoned_mutex);
        *sender_guard = None;
    }

    #[test]
    fn test_hotkey_window_proc_forwards_display_change_events() {
        let _guard = HOTKEY_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);

        let (tx, rx) = mpsc::channel::<WindowEvent>();
        set_display_change_sender(tx).unwrap();

        let lresult = hotkey_window_proc_inner(
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
    fn test_hotkey_handle_drop_does_not_block_when_quit_post_fails() {
        let _guard = HOTKEY_TEST_LOCK
            .lock()
            .unwrap_or_else(recover_poisoned_mutex);

        let sleeping_thread = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(1));
        });

        let start = std::time::Instant::now();
        drop(HotkeyHandle {
            hwnd: HWND(std::ptr::null_mut()),
            thread_id: 0,
            thread: Some(sleeping_thread),
            registered_ids: Vec::new(),
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

        // Navigation
        assert_eq!(parse_vk("Left"), Some(vk::LEFT));
        assert_eq!(parse_vk("RIGHT"), Some(vk::RIGHT));

        // Special keys
        assert_eq!(parse_vk("Tab"), Some(vk::TAB));
        assert_eq!(parse_vk("Space"), Some(vk::SPACE));
        assert_eq!(parse_vk("Enter"), Some(vk::ENTER));
        assert_eq!(parse_vk("Escape"), Some(vk::ESCAPE));

        // Invalid
        assert_eq!(parse_vk("Invalid"), None);
        assert_eq!(parse_vk("F13"), None);
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
    }

    #[test]
    fn test_modifiers_to_win32() {
        let mods = Modifiers::win();
        let flags = mods.to_win32();
        assert!(flags.contains(MOD_WIN));
        assert!(flags.contains(MOD_NOREPEAT));
        assert!(!flags.contains(MOD_CONTROL));

        let mods = Modifiers {
            ctrl: true,
            alt: true,
            shift: true,
            win: false,
        };
        let flags = mods.to_win32();
        assert!(flags.contains(MOD_CONTROL));
        assert!(flags.contains(MOD_ALT));
        assert!(flags.contains(MOD_SHIFT));
        assert!(!flags.contains(MOD_WIN));
    }
}
