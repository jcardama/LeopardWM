#![windows_subsystem = "windows"]
//! LeopardWM Daemon
//!
//! Main daemon process for the LeopardWM window manager.
//!
//! Responsibilities:
//! - Maintain workspace state
//! - Process window events from the platform layer
//! - Handle IPC commands from the CLI
//! - Trigger layout recalculations
//! - Apply window placements
//! - System tray icon and menu

mod animation_worker;
mod command_handler;
mod config;
mod drag;
mod event_handler;
mod events;
mod helpers;
mod ipc_server;
mod layout_apply;
mod monitors;
mod persistence;
mod scratchpad;
mod settings;
mod sticky;
mod startup;
mod state;
#[cfg(test)]
mod tests;
mod transitions;
mod tray;
mod ui_sync;
mod update_check;
mod window_rules;

use ipc_server::*;
use startup::*;
use state::*;

use anyhow::Result;
use clap::Parser;
use config::Config;
use leopardwm_core_layout::Rect;
use leopardwm_ipc::{pipe_name_candidates, preferred_pipe_name, IpcCommand, IpcResponse};
use leopardwm_platform_win32::{
    cascade_windows, enumerate_monitors, enumerate_windows, install_event_hooks,
    install_mouse_hook, overlay::OverlayWindow, parse_hotkey_string, register_gestures,
    register_hotkeys, restore_windows_moved_offscreen, set_display_change_sender,
    set_dpi_awareness, set_power_state_sender, uncloak_all_visible_windows,
    GestureEvent, Hotkey, HotkeyId,
    MonitorId, MonitorInfo, WindowEvent,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

/// Command-line arguments for the daemon binary.
#[derive(Parser, Debug, Clone)]
#[command(name = "leopardwm", about = "LeopardWM tiling window manager daemon")]
pub struct Args {
    /// Disable global hotkey registration
    #[arg(long)]
    pub no_hotkeys: bool,
    /// Safe mode: disables global hotkey registration
    #[arg(long)]
    pub safe_mode: bool,
}

impl Args {
    /// Returns true if hotkeys should be skipped (either --no-hotkeys or --safe-mode).
    pub fn skip_hotkeys(&self) -> bool {
        self.no_hotkeys || self.safe_mode
    }
}

pub(crate) use events::DaemonEvent;


/// Retry count for shutdown visibility recovery when an apply worker fails to exit in time.
const SHUTDOWN_RECOVERY_RETRY_ATTEMPTS: usize = 3;
/// Delay between additional shutdown visibility recovery attempts.
const SHUTDOWN_RECOVERY_RETRY_DELAY: Duration = Duration::from_millis(250);
/// Final bounded wait per lingering apply worker before daemon exit.
const SHUTDOWN_FINAL_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownMode {
    Graceful,
    PanicRevert,
}

impl ShutdownMode {
    fn should_save_state(self) -> bool {
        matches!(self, Self::Graceful)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Graceful => "graceful",
            Self::PanicRevert => "panic_revert",
        }
    }
}

fn shutdown_mode_for_command(cmd: &IpcCommand) -> Option<ShutdownMode> {
    match cmd {
        IpcCommand::Stop => Some(ShutdownMode::Graceful),
        IpcCommand::PanicRevert => Some(ShutdownMode::PanicRevert),
        _ => None,
    }
}
/// Hotkey registration result containing handle and mapping.
struct HotkeyState {
    /// Handle to unregister hotkeys on drop.
    handle: Option<leopardwm_platform_win32::HotkeyHandle>,
    /// Mapping of hotkey IDs to commands.
    mapping: HashMap<HotkeyId, IpcCommand>,
    /// Number of hotkeys that were requested for registration.
    requested_count: usize,
    /// Number of hotkeys the OS actually registered (may be less than
    /// `requested_count` if some conflicted with other applications).
    registered_count: usize,
}

/// Register hotkeys from config and return state.
///
/// This function is called both at startup and on config reload.
/// Build the tray quick-toggle state from config (reads autostart from the registry).
fn quick_toggle_state(config: &Config) -> tray::QuickToggleState {
    tray::QuickToggleState {
        active_border: config.appearance.active_border,
        focus_new_windows: config.behavior.focus_new_windows,
        focus_follows_mouse: config.behavior.focus_follows_mouse,
        auto_start: leopardwm_platform_win32::autostart::get_autostart().unwrap_or(false),
        centering_mode: match config.layout.centering_mode {
            config::CenteringModeConfig::Center => tray::CENTERING_CENTER,
            config::CenteringModeConfig::JustInView => tray::CENTERING_JUST_IN_VIEW,
            config::CenteringModeConfig::OnOverflow => tray::CENTERING_ON_OVERFLOW,
        },
        placement_mode: match config.behavior.new_window_placement {
            config::NewWindowPlacement::NewColumn => tray::PLACEMENT_NEW_COLUMN,
            config::NewWindowPlacement::InColumn => tray::PLACEMENT_IN_COLUMN,
        },
    }
}

/// Sync tray quick-toggle check marks with the current config.
fn sync_tray_toggles(tray_manager: &Option<tray::TrayManager>, config: &Config) {
    if let Some(ref mgr) = tray_manager {
        mgr.update_quick_toggles(&quick_toggle_state(config));
    }
}

/// Reload hotkeys and sync tray/overlay state after a config change.
///
/// Called from IPC Reload, tray Reload, and settings save handlers.
async fn reload_config_and_hotkeys(
    state: &Arc<Mutex<AppState>>,
    hotkey_state: &mut HotkeyState,
    event_tx: &mpsc::Sender<DaemonEvent>,
    tray_manager: &Option<tray::TrayManager>,
    snap_hint_overlay: &Option<OverlayWindow>,
) {
    hotkey_state.handle = None;
    let new_config = {
        let state = state.lock().await;
        state.config.clone()
    };
    *hotkey_state = setup_hotkeys(&new_config, event_tx.clone());
    sync_tray_toggles(tray_manager, &new_config);
    if let Some(ref overlay) = snap_hint_overlay {
        overlay.set_opacity(new_config.snap_hints.opacity);
    }
}

fn setup_hotkeys(config: &Config, event_tx: mpsc::Sender<DaemonEvent>) -> HotkeyState {
    let config_hotkeys = &config.hotkeys.bindings;

    // Build hotkey definitions and command mapping. IDs are intrinsic to
    // each (modifiers, vk) combo via `Hotkey::stable_id`, NOT sequential —
    // so a config reload can never remap an existing ID to a different
    // command and have a queued WM_HOTKEY fire the wrong action.
    let mut hotkeys = Vec::new();
    let mut mapping = HashMap::new();

    for (key_str, cmd_str) in config_hotkeys {
        if let Some((modifiers, vk)) = parse_hotkey_string(key_str) {
            if let Some(cmd) = config::parse_command(cmd_str) {
                let id = Hotkey::stable_id(modifiers, vk);
                if mapping.contains_key(&id) {
                    warn!(
                        "Duplicate hotkey combo for {} (id {}); ignoring the second binding",
                        key_str, id
                    );
                    continue;
                }
                hotkeys.push(Hotkey::new(id, modifiers, vk));
                mapping.insert(id, cmd);
                debug!("Configured hotkey {}: {} -> {:?}", id, key_str, cmd_str);
            } else {
                warn!(
                    "Unknown command in hotkey config: {} -> {}",
                    key_str, cmd_str
                );
            }
        } else {
            warn!("Invalid hotkey string in config: {}", key_str);
        }
    }

    let requested_count = hotkeys.len();

    if hotkeys.is_empty() {
        info!("No hotkeys configured");
        return HotkeyState {
            handle: None,
            mapping,
            requested_count: 0,
            registered_count: 0,
        };
    }

    match register_hotkeys(hotkeys) {
        Ok((handle, hotkey_receiver)) => {
            info!("Registered {} global hotkeys", handle.registered_count());

            // Spawn task to forward hotkey events
            match std::thread::Builder::new()
                .name("hotkey-fwd".to_string())
                .spawn(move || {
                    while let Ok(event) = hotkey_receiver.recv() {
                        if event_tx.blocking_send(DaemonEvent::Hotkey(event)).is_err() {
                            break;
                        }
                    }
                }) {
                Ok(_) => {} // Thread is detached, we don't track it
                Err(e) => {
                    warn!("Failed to spawn hotkey-fwd thread: {}", e);
                }
            }

            let registered_count = handle.registered_count();
            HotkeyState {
                handle: Some(handle),
                mapping,
                requested_count,
                registered_count,
            }
        }
        Err(e) => {
            warn!(
                "Failed to register hotkeys: {}. Global shortcuts disabled.",
                e
            );
            HotkeyState {
                handle: None,
                mapping,
                requested_count,
                registered_count: 0,
            }
        }
    }
}

/// Shared shutdown/recovery cleanup used by all daemon exit paths.
async fn run_shutdown_cleanup(state: &Arc<Mutex<AppState>>, mode: ShutdownMode) {
    info!("Running {} shutdown cleanup", mode.label());

    let (managed_window_ids, pending_apply_workers, apply_timeout) = {
        let mut state = state.lock().await;
        // Restore WS_MAXIMIZEBOX before visibility recovery so Windows allows resize
        state.restore_snap_for_all_windows();
        let pending_apply_workers = state.begin_shutdown_or_revert();
        if mode.should_save_state() {
            if let Err(e) = state.save_state() {
                warn!("Failed to save workspace state: {}", e);
            }
        }
        (
            state.all_managed_window_ids(),
            pending_apply_workers,
            state.layout_apply_timeout,
        )
    };

    let mut pending_workers = pending_apply_workers
        .into_iter()
        .map(Some)
        .collect::<Vec<_>>();

    let mut timed_out_workers = 0usize;
    for worker in &mut pending_workers {
        if !join_with_timeout(worker, apply_timeout) {
            warn!(
                "Timed-out apply worker did not exit before {} cleanup; continuing with best effort",
                mode.label()
            );
            timed_out_workers += 1;
        }
    }
    pending_workers.retain(Option::is_some);

    run_visibility_recovery_pass(&managed_window_ids, mode.label());

    if !pending_workers.is_empty() {
        for attempt in 1..=SHUTDOWN_RECOVERY_RETRY_ATTEMPTS {
            tokio::time::sleep(SHUTDOWN_RECOVERY_RETRY_DELAY).await;
            for worker in &mut pending_workers {
                let _ = join_with_timeout(worker, SHUTDOWN_RECOVERY_RETRY_DELAY);
            }
            pending_workers.retain(Option::is_some);
            info!(
                "Running additional {} cleanup visibility recovery pass {}/{} after {} timed-out apply worker(s)",
                mode.label(),
                attempt,
                SHUTDOWN_RECOVERY_RETRY_ATTEMPTS,
                timed_out_workers.max(pending_workers.len())
            );
            run_visibility_recovery_pass(&managed_window_ids, mode.label());
            if pending_workers.is_empty() {
                break;
            }
        }
    }

    if !pending_workers.is_empty() {
        warn!(
            "{} timed-out apply worker(s) still running after {} cleanup retries; running final bounded joins before exit",
            pending_workers.len(),
            mode.label()
        );
        for worker in &mut pending_workers {
            let _ = join_with_timeout(worker, SHUTDOWN_FINAL_JOIN_TIMEOUT);
        }
        pending_workers.retain(Option::is_some);
        run_visibility_recovery_pass(&managed_window_ids, mode.label());
        if !pending_workers.is_empty() {
            warn!(
                "{} timed-out apply worker(s) still running after final {} bounded joins ({} ms each); exiting without detached recovery threads",
                pending_workers.len(),
                mode.label(),
                SHUTDOWN_FINAL_JOIN_TIMEOUT.as_millis()
            );
        }
    }
}

/// Lightweight DwmFlush-aligned animation loop for resize preview transitions.
/// Directly repositions the overlay window via SetWindowPos — no channel round-trip.
fn resize_preview_animation_loop(
    overlay_hwnd: isize,
    start: leopardwm_core_layout::Rect,
    target: leopardwm_core_layout::Rect,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    active: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    use windows::Win32::Graphics::Dwm::DwmFlush;

    active.store(true, Ordering::Release);
    let start_time = std::time::Instant::now();
    let duration_ms = crate::state::RESIZE_PREVIEW_DURATION_MS;

    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let elapsed = start_time.elapsed().as_millis() as u64;
        let done = elapsed >= duration_ms;
        let t = (elapsed as f64 / duration_ms as f64).clamp(0.0, 1.0);
        // Cubic ease-out
        let t = 1.0 - (1.0 - t).powi(3);
        let rect = leopardwm_core_layout::Rect::new(
            crate::state::lerp_i32(start.x, target.x, t),
            crate::state::lerp_i32(start.y, target.y, t),
            crate::state::lerp_i32(start.width, target.width, t),
            crate::state::lerp_i32(start.height, target.height, t),
        );
        // Direct SetWindowPos — zero indirection, zero channel latency.
        leopardwm_platform_win32::overlay::reposition_overlay(overlay_hwnd, rect);
        if done {
            break;
        }
        // Wait for next vsync — smooth timing without busy-spinning.
        let _ = unsafe { DwmFlush() };
    }

    active.store(false, Ordering::Release);
}

/// Borrowed event-loop state shared by the main-loop event handlers.
struct EventLoopCtx<'a> {
    state: &'a Arc<Mutex<AppState>>,
    event_tx: &'a mpsc::Sender<DaemonEvent>,
    hotkey_state: &'a mut HotkeyState,
    tray_manager: &'a Option<tray::TrayManager>,
    snap_hint_overlay: &'a Option<OverlayWindow>,
    settings_sync_tx: &'a std::sync::mpsc::Sender<settings::SettingsEvent>,
    settings_handle: &'a mut Option<settings::SettingsWindowHandle>,
    animation_worker: &'a animation_worker::AnimationWorkerHandle,
    animation_active: &'a mut bool,
    last_frame_instant: &'a mut Option<std::time::Instant>,
    snap_hint_timer_handle: &'a mut Option<tokio::task::JoinHandle<()>>,
    focus_follows_mouse_timer: &'a mut Option<tokio::task::JoinHandle<()>>,
    display_change_timer: &'a mut Option<tokio::task::JoinHandle<()>>,
}

/// Set DPI awareness, ensure a config file exists, load and validate config, and init logging.
fn bootstrap_config() -> Result<(Config, Vec<config::ConfigWarning>)> {
    // Set DPI awareness before any window/GDI operations
    if set_dpi_awareness() {
        eprintln!("[leopardwm] DPI awareness set to Per-Monitor Aware V2");
    } else {
        eprintln!("[leopardwm] Warning: Failed to set DPI awareness (may already be set)");
    }

    // Auto-create config file on first run
    match config::ensure_config_on_disk() {
        Ok(Some(path)) => eprintln!("[leopardwm] Created default config: {}", path.display()),
        Ok(None) => {}
        Err(e) => eprintln!("[leopardwm] Warning: Could not create config file: {}", e),
    }

    // Load configuration first (needed for log level)
    let mut config = Config::load().unwrap_or_else(|e| {
        // Can't use tracing yet, fall back to eprintln
        eprintln!("Failed to load configuration: {}. Using defaults.", e);
        Config::default()
    });

    // Initialize logging with configured log level
    let log_level = match config.behavior.log_level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO, // default fallback for invalid values
    };
    let subscriber = FmtSubscriber::builder().with_max_level(log_level).finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Validate and clamp config values
    let config_warnings = config.validate();
    for w in &config_warnings {
        warn!("Config: {} - {}", w.field, w.message);
    }

    Ok((config, config_warnings))
}

/// Install a panic hook that uncloaks all windows and writes a crash report.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("[leopardwm] PANIC detected — emergency uncloaking all windows");
        leopardwm_platform_win32::restore_maximizebox_panic_recovery();
        uncloak_all_visible_windows();
        match enumerate_windows() {
            Ok(windows) => {
                let window_ids: Vec<u64> = windows.into_iter().map(|w| w.hwnd).collect();
                match restore_windows_moved_offscreen(&window_ids) {
                    Ok(restored) => {
                        if restored > 0 {
                            eprintln!(
                                "[leopardwm] Restored {} MoveOffScreen window(s) in panic recovery",
                                restored
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("[leopardwm] MoveOffScreen panic recovery failed: {}", e);
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "[leopardwm] Failed to enumerate windows for MoveOffScreen recovery: {}",
                    e
                );
            }
        }

        // Write crash report to temp dir
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let crash_path = std::env::temp_dir().join(format!("leopardwm-crash-{}.txt", ts));
        let report = format_crash_report(info);
        if let Err(e) = std::fs::write(&crash_path, &report) {
            eprintln!("[leopardwm] Failed to write crash report: {}", e);
        } else {
            eprintln!(
                "[leopardwm] Crash report written to: {}",
                crash_path.display()
            );
        }

        default_hook(info);
    }));
}

/// Detect all monitors, falling back to a single default monitor on failure.
fn detect_monitors() -> Vec<MonitorInfo> {
    match enumerate_monitors() {
        Ok(monitors) if !monitors.is_empty() => {
            info!("Detected {} monitor(s):", monitors.len());
            for m in &monitors {
                info!(
                    "  Monitor {}: {}x{} (work area: {}x{} at {},{}) DPI {:.0}%{} \"{}\"",
                    m.id,
                    m.rect.width,
                    m.rect.height,
                    m.work_area.width,
                    m.work_area.height,
                    m.work_area.x,
                    m.work_area.y,
                    m.scale_factor * 100.0,
                    if m.is_primary { " [PRIMARY]" } else { "" },
                    m.device_name
                );
            }
            monitors
        }
        Ok(_) | Err(_) => {
            warn!(
                "Failed to detect monitors, using fallback {}x{}",
                FALLBACK_VIEWPORT_WIDTH, FALLBACK_VIEWPORT_HEIGHT
            );
            vec![MonitorInfo {
                id: 1,
                rect: Rect::new(0, 0, FALLBACK_VIEWPORT_WIDTH, FALLBACK_VIEWPORT_HEIGHT),
                work_area: Rect::new(0, 0, FALLBACK_VIEWPORT_WIDTH, FALLBACK_WORK_AREA_HEIGHT),
                is_primary: true,
                device_name: "Fallback".to_string(),
                scale_factor: 1.0,
            }]
        }
    }
}

/// Enumerate existing windows, restore saved workspace state, and normalize startup layout.
fn init_workspace_state(state: &mut AppState) {
    match state.enumerate_and_add_windows() {
        Ok(count) => {
            info!("Found and added {} manageable windows", count);
        }
        Err(e) => {
            error!("Failed to enumerate windows: {}", e);
        }
    }

    // Log workspace state for all monitors
    let total_windows: usize = state.workspaces.values()
        .flat_map(|ws_vec| ws_vec.iter())
        .map(|w| w.window_count()).sum();
    let total_columns: usize = state.workspaces.values()
        .flat_map(|ws_vec| ws_vec.iter())
        .map(|w| w.column_count()).sum();
    info!(
        "Workspaces initialized across {} monitors: {} total columns, {} total windows",
        state.workspaces.len(),
        total_columns,
        total_windows
    );

    // Restore saved workspace state (after windows are enumerated so scroll
    // offsets aren't clamped against empty workspaces).
    let _restored_monitors = if let Some(snapshot) = AppState::load_state() {
        let restored = state.restore_state(&snapshot);
        info!("Restored workspace state from previous session");
        restored
    } else {
        HashSet::new()
    };

    // Collect viewport widths first to avoid borrow issues
    let monitor_widths: HashMap<MonitorId, i32> = state
        .monitors
        .iter()
        .map(|(id, m)| (*id, m.work_area.width))
        .collect();

    // Ensure the focused column is visible in the viewport for every workspace.
    // This also corrects stale scroll offsets from restored state that no longer
    // match the current window set.
    for (monitor_id, ws_vec) in state.workspaces.iter_mut() {
        for workspace in ws_vec.iter_mut() {
            if workspace.column_count() > 0 {
                let width = monitor_widths
                    .get(monitor_id)
                    .copied()
                    .unwrap_or(FALLBACK_VIEWPORT_WIDTH);
                workspace.ensure_focused_visible(width);
            }
        }
    }

    // Normalize all column widths to the first width preset on startup.
    // Windows may have arbitrary sizes before tiling; using a uniform width
    // ensures consistent initial layout.
    let monitor_widths_for_default: HashMap<_, _> = state
        .monitors
        .iter()
        .map(|(&id, m)| {
            let vw = m.work_area.width;
            (id, state.config.layout.default_column_width_px(vw))
        })
        .collect();
    for (&monitor_id, ws_vec) in state.workspaces.iter_mut() {
        let default_width = monitor_widths_for_default
            .get(&monitor_id)
            .copied()
            .unwrap_or(800);
        for workspace in ws_vec.iter_mut() {
            workspace.set_all_column_widths(default_width);
        }
    }

    // Reset scroll offset to 0 so windows tile from the left edge on startup
    // (like niri). The ensure_focused_visible call above may leave a stale
    // centered offset when restoring state.
    for (_monitor_id, ws_vec) in state.workspaces.iter_mut() {
        for workspace in ws_vec.iter_mut() {
            workspace.set_scroll_offset(0.0);
        }
    }
}

/// Wire up the tab-strip action and rename-dialog channels and their forwarder threads.
async fn spawn_tab_forwarders(
    state: &Arc<Mutex<AppState>>,
    event_tx: &mpsc::Sender<DaemonEvent>,
    thread_handles: &mut Vec<std::thread::JoinHandle<()>>,
) {
    // Tab strip overlay action channel. The overlay's WndProc posts a
    // `TabActionEvent` on WM_LBUTTONDOWN / WM_MBUTTONUP / WM_RBUTTONUP /
    // close-X; the forwarder thread below dispatches a `DaemonEvent::TabAction`
    // carrying the captured column identity through the main event queue.
    let (tab_action_tx, tab_action_rx) =
        std::sync::mpsc::channel::<leopardwm_platform_win32::tab_strip::TabActionEvent>();
    {
        let mut state_guard = state.lock().await;
        state_guard.install_tab_strip(tab_action_tx);
    }
    {
        let event_tx_for_actions = event_tx.clone();
        match std::thread::Builder::new()
            .name("tab-action-forwarder".into())
            .spawn(move || {
                while let Ok(action_event) = tab_action_rx.recv() {
                    if event_tx_for_actions
                        .blocking_send(DaemonEvent::TabAction {
                            monitor: action_event.monitor,
                            workspace_idx: action_event.workspace_idx,
                            column_idx: action_event.column_idx,
                            tab_idx: action_event.tab_idx,
                            action: action_event.action,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }) {
            Ok(handle) => thread_handles.push(handle),
            Err(e) => warn!("Failed to spawn tab-action forwarder thread: {}", e),
        }
    }

    // Rename-dialog result channel. The Rename arm spawns a modal
    // dialog on its own short-lived thread; that thread posts a
    // `TabRenameResult` here when the user OKs or cancels. This
    // forwarder converts the result into a `DaemonEvent::TabRenameSubmitted`
    // so the daemon writes the override under the main event loop's
    // serial lock (no races with concurrent layout work).
    let (rename_result_tx, rename_result_rx) =
        std::sync::mpsc::channel::<crate::events::TabRenameResult>();
    {
        let mut state_guard = state.lock().await;
        state_guard.rename_result_tx = Some(rename_result_tx);
    }
    {
        let event_tx_for_rename = event_tx.clone();
        match std::thread::Builder::new()
            .name("tab-rename-forwarder".into())
            .spawn(move || {
                while let Ok(result) = rename_result_rx.recv() {
                    if event_tx_for_rename
                        .blocking_send(DaemonEvent::TabRenameSubmitted {
                            monitor: result.monitor,
                            workspace_idx: result.workspace_idx,
                            column_idx: result.column_idx,
                            tab_idx: result.tab_idx,
                            target_hwnd: result.target_hwnd,
                            new_title: result.new_title,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }) {
            Ok(handle) => thread_handles.push(handle),
            Err(e) => warn!("Failed to spawn tab-rename forwarder thread: {}", e),
        }
    }
}

/// Install WinEvent hooks and register display-change and power-state forwarding.
fn setup_window_hooks(
    config: &Config,
    event_tx: &mpsc::Sender<DaemonEvent>,
    thread_handles: &mut Vec<std::thread::JoinHandle<()>>,
) -> Option<leopardwm_platform_win32::EventHookHandle> {
    let hook_handle = if config.behavior.track_focus_changes {
        match install_event_hooks() {
            Ok((handle, event_receiver)) => {
                info!("WinEvent hooks installed");

                // Spawn task to forward window events from std::sync::mpsc to tokio channel
                match spawn_forwarding_thread(
                    "winevent-fwd",
                    event_receiver,
                    event_tx.clone(),
                    DaemonEvent::WindowEvent,
                ) {
                    Ok(handle) => thread_handles.push(handle),
                    Err(e) => warn!("{}", e),
                }

                Some(handle)
            }
            Err(e) => {
                warn!(
                    "Failed to install WinEvent hooks: {}. Window tracking disabled.",
                    e
                );
                None
            }
        }
    } else {
        info!("WinEvent hooks disabled by config (track_focus_changes = false)");
        None
    };

    // Register display change sender for WM_DISPLAYCHANGE events
    // This allows the hotkey window to forward display changes to our event loop
    {
        let (display_tx, display_rx) = std::sync::mpsc::channel::<WindowEvent>();
        if let Err(e) = set_display_change_sender(display_tx) {
            warn!("Failed to register display change sender: {}. Display changes may not be detected.", e);
        } else {
            // Forward display change events to the daemon event loop
            match spawn_forwarding_thread(
                "display-fwd",
                display_rx,
                event_tx.clone(),
                DaemonEvent::WindowEvent,
            ) {
                Ok(handle) => thread_handles.push(handle),
                Err(e) => warn!("{}", e),
            }
            info!("Display change detection enabled");
        }
    }

    // Register power state sender for WM_POWERBROADCAST events
    // Forwards AC/battery and power saver state changes to the daemon event loop
    {
        let (power_tx, power_rx) = std::sync::mpsc::channel::<bool>();
        if let Err(e) = set_power_state_sender(power_tx) {
            warn!("Failed to register power state sender: {}. Power state changes may not be detected.", e);
        } else {
            match spawn_forwarding_thread(
                "power-fwd",
                power_rx,
                event_tx.clone(),
                |on_battery_or_saver| DaemonEvent::PowerStateChanged { on_battery_or_saver },
            ) {
                Ok(handle) => thread_handles.push(handle),
                Err(e) => warn!("{}", e),
            }
            info!("Power state detection enabled");
        }
    }

    hook_handle
}

/// Install the mouse hook for focus-follows-mouse (if enabled).
fn setup_mouse_hook(
    config: &Config,
    event_tx: &mpsc::Sender<DaemonEvent>,
    thread_handles: &mut Vec<std::thread::JoinHandle<()>>,
) -> Option<leopardwm_platform_win32::MouseHookHandle> {
    if config.behavior.focus_follows_mouse {
        let (mouse_tx, mouse_rx) = std::sync::mpsc::channel::<WindowEvent>();
        match install_mouse_hook(mouse_tx) {
            Ok(handle) => {
                info!(
                    "Focus-follows-mouse enabled (delay: {}ms)",
                    config.behavior.focus_follows_mouse_delay_ms
                );

                // Forward mouse events to the daemon event loop
                match spawn_forwarding_thread(
                    "mouse-fwd",
                    mouse_rx,
                    event_tx.clone(),
                    DaemonEvent::WindowEvent,
                ) {
                    Ok(handle) => thread_handles.push(handle),
                    Err(e) => warn!("{}", e),
                }

                Some(handle)
            }
            Err(e) => {
                warn!(
                    "Failed to install mouse hook: {}. Focus-follows-mouse disabled.",
                    e
                );
                None
            }
        }
    } else {
        info!("Focus-follows-mouse disabled by config (focus_follows_mouse = false)");
        None
    }
}

/// Register gesture detection (if enabled).
fn setup_gestures(
    config: &Config,
    event_tx: &mpsc::Sender<DaemonEvent>,
    thread_handles: &mut Vec<std::thread::JoinHandle<()>>,
) -> Option<leopardwm_platform_win32::GestureHandle> {
    if config.gestures.enabled {
        // Set scroll modifier before registering the hook
        leopardwm_platform_win32::set_scroll_modifier(&config.hotkeys.scroll_modifier);

        match register_gestures() {
            Ok((handle, gesture_receiver)) => {
                info!("Gesture detection enabled");

                // Spawn thread to forward gesture events
                match spawn_forwarding_thread(
                    "gesture-fwd",
                    gesture_receiver,
                    event_tx.clone(),
                    DaemonEvent::Gesture,
                ) {
                    Ok(handle) => thread_handles.push(handle),
                    Err(e) => warn!("{}", e),
                }

                Some(handle)
            }
            Err(e) => {
                warn!(
                    "Failed to register gestures: {}. Gesture support disabled.",
                    e
                );
                None
            }
        }
    } else {
        info!("Gesture detection disabled by config (gestures.enabled = false)");
        None
    }
}

/// Initialize the system tray icon and bridge its events into the event loop.
fn setup_tray(
    config: &Config,
    event_tx: &mpsc::Sender<DaemonEvent>,
    thread_handles: &mut Vec<std::thread::JoinHandle<()>>,
) -> Option<tray::TrayManager> {
    let (tray_sync_tx, tray_sync_rx) = std::sync::mpsc::channel();

    // Spawn task to forward tray events from sync channel to async channel
    match spawn_forwarding_thread(
        "tray-fwd",
        tray_sync_rx,
        event_tx.clone(),
        DaemonEvent::Tray,
    ) {
        Ok(handle) => thread_handles.push(handle),
        Err(e) => warn!("{}", e),
    }

    let initial_toggles = quick_toggle_state(config);
    match tray::TrayManager::new(tray_sync_tx, initial_toggles) {
        Ok(manager) => {
            info!("System tray icon initialized");
            Some(manager)
        }
        Err(e) => {
            warn!("Failed to create system tray icon: {}. Tray disabled.", e);
            None
        }
    }
}

/// Print the startup banner for immediate user feedback.
async fn print_banner(
    state: &Arc<Mutex<AppState>>,
    config_warnings: &[config::ConfigWarning],
    hotkey_state: &HotkeyState,
    args: &Args,
) {
    let state = state.lock().await;
    let monitors_ordered: Vec<_> = state.monitors.values().collect();
    let monitor_names: Vec<String> = monitors_ordered
        .iter()
        .map(|m| m.device_name.clone())
        .collect();
    let monitor_dpi: Vec<f64> = monitors_ordered
        .iter()
        .map(|m| m.scale_factor)
        .collect();
    let window_count: usize = state.workspaces.values()
        .flat_map(|ws_vec| ws_vec.iter())
        .map(|w| w.window_count()).sum();
    let config_path = config::config_paths()
        .into_iter()
        .find(|p| p.exists())
        .map(|p| p.display().to_string());
    let log_path = std::env::temp_dir()
        .join("leopardwm-daemon.log")
        .display()
        .to_string();
    print_startup_banner(&StartupInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        monitor_names,
        monitor_dpi,
        window_count,
        hotkeys_registered: hotkey_state.registered_count,
        hotkeys_requested: hotkey_state.requested_count,
        config_path,
        config_warnings: config_warnings
            .iter()
            .map(|w| format!("{}: {}", w.field, w.message))
            .collect(),
        log_path,
        safe_mode: args.safe_mode,
        no_hotkeys: args.skip_hotkeys(),
        reduce_motion: state.reduce_motion,
        on_battery_or_saver: state.on_battery_or_saver,
        high_contrast: state.high_contrast,
    });
}

/// Handle an IPC Subscribe: build the ack + snapshot + receiver bundle atomically.
async fn handle_ipc_subscribe(
    state: &Arc<Mutex<AppState>>,
    events: std::collections::BTreeSet<leopardwm_ipc::EventKind>,
    responder: tokio::sync::oneshot::Sender<events::SubscribeStartup>,
) {
    // Build the (ack + snapshot + receiver) bundle atomically
    // under the AppState mutex so any event emitted after the
    // receiver is created is guaranteed to land in `rx`, and
    // the snapshot reflects exactly that instant. See
    // `events::SubscribeStartup` for the contract.
    use leopardwm_ipc::EventKind;
    use leopardwm_platform_win32::get_process_executable;
    let s = state.lock().await;
    let receiver = s.event_broadcaster.subscribe();
    let mut snapshot = Vec::new();

    if events.contains(&EventKind::Workspace) {
        for (monitor, &idx) in &s.active_workspace {
            snapshot.push(leopardwm_ipc::IpcEvent::WorkspaceChanged {
                monitor: *monitor as i64,
                old_index: idx as u8,
                new_index: idx as u8,
                name: s.config.workspaces.name_for(idx),
            });
        }
    }
    if events.contains(&EventKind::FocusedWindow) {
        let hwnd = s.previous_focused_hwnd;
        let monitor = s.focused_monitor as i64;
        let (title, class_name, executable) = if let Some(h) = hwnd {
            match s.lookup_window_info(h) {
                Some(i) => (
                    Some(i.title.clone()),
                    Some(i.class_name.clone()),
                    Some(get_process_executable(i.process_id).unwrap_or_default()),
                ),
                None => (None, None, None),
            }
        } else {
            (None, None, None)
        };
        snapshot.push(leopardwm_ipc::IpcEvent::FocusedWindowChanged {
            monitor,
            hwnd,
            title,
            class_name,
            executable,
        });
    }
    if events.contains(&EventKind::Layout) {
        let monitor = s.focused_monitor;
        let workspace_index = s.active_workspace_idx(monitor);
        let focused_column = s
            .workspaces
            .get(&monitor)
            .and_then(|list| list.get(workspace_index))
            .map(|ws| ws.focused_column_index());
        snapshot.push(leopardwm_ipc::IpcEvent::LayoutChanged {
            monitor: monitor as i64,
            workspace_index: workspace_index as u8,
            focused_column,
            columns: s.focused_layout_columns(),
        });
    }

    let ack = leopardwm_ipc::IpcResponse::Subscribed {
        events: events.clone(),
    };
    drop(s);

    let _ = responder.send(crate::events::SubscribeStartup {
        ack,
        snapshot,
        receiver,
    });
}

/// Handle an IPC command; returns true when the daemon should shut down.
async fn handle_ipc_command(
    ctx: &mut EventLoopCtx<'_>,
    cmd: IpcCommand,
    responder: tokio::sync::oneshot::Sender<IpcResponse>,
) -> bool {
    if let Some(mode) = shutdown_mode_for_command(&cmd) {
        if mode == ShutdownMode::PanicRevert {
            warn!("IPC: panic_revert requested");
        } else {
            info!("IPC: stop requested");
        }
        if responder.send(IpcResponse::Ok).is_err() {
            debug!(
                "Client disconnected before receiving {} response",
                mode.label()
            );
        }
        run_shutdown_cleanup(ctx.state, mode).await;
        return true;
    }

    let is_reload = matches!(&cmd, IpcCommand::Reload);
    let is_resize = matches!(&cmd, IpcCommand::Resize { .. });
    let is_toggle_pause = matches!(&cmd, IpcCommand::TogglePause);

    let (response, should_animate, column_rect, hint_duration) = {
        let mut state = ctx.state.lock().await;
        let response = state.handle_command(cmd);
        let animating = state.is_animating();

        // Get column rect for snap hint if this is a resize
        let rect = if is_resize && state.config.snap_hints.enabled {
            state.get_focused_column_rect()
        } else {
            None
        };
        let duration = state.config.snap_hints.duration_ms;

        (response, animating, rect, duration)
    };

    // If config was reloaded successfully, also reload hotkeys
    if is_reload && matches!(response, IpcResponse::Ok) {
        reload_config_and_hotkeys(
            ctx.state, ctx.hotkey_state, ctx.event_tx,
            ctx.tray_manager, ctx.snap_hint_overlay,
        ).await;
        info!("Hotkeys reloaded after config reload");
    }

    // Log if client disconnected before receiving response
    if responder.send(response).is_err() {
        debug!("Client disconnected before receiving IPC response");
    }

    if is_toggle_pause {
        let state = ctx.state.lock().await;
        if let Some(ref mgr) = ctx.tray_manager {
            mgr.update_pause_text(state.paused);
            let wc = state.all_managed_window_ids().len();
            let mc = state.monitors.len();
            let aws = (state.active_workspace_idx(state.focused_monitor) + 1) as u8;
            mgr.update_tooltip(
                wc,
                mc,
                state.paused,
                Some((ctx.hotkey_state.registered_count, ctx.hotkey_state.requested_count)),
                aws,
            );
        }
    }

    // Show snap hint for resize operations
    if is_resize {
        if let (Some(ref overlay), Some(rect)) = (ctx.snap_hint_overlay, column_rect) {
            // Cancel any pending hide timer
            if let Some(handle) = ctx.snap_hint_timer_handle.take() {
                handle.abort();
            }

            // Show the snap hint
            overlay.show_snap_target(rect);

            // Schedule hide after duration
            let hide_tx = ctx.event_tx.clone();
            let duration = hint_duration;
            *ctx.snap_hint_timer_handle = Some(tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(duration as u64))
                    .await;
                let _ = hide_tx.send(DaemonEvent::HideSnapHint).await;
            }));
        }
    }

    // Start animation if needed
    if should_animate && !*ctx.animation_active {
        let mut state = ctx.state.lock().await;
        state.tick_animations(0);
        if let Ok(true) = state.send_animation_frame(ctx.animation_worker) {
            *ctx.animation_active = true;
            *ctx.last_frame_instant = Some(std::time::Instant::now());
        }
    }

    false
}

/// Handle a platform window event, including display-change and focus-follows-mouse debouncing.
async fn process_window_event(ctx: &mut EventLoopCtx<'_>, win_event: WindowEvent) {
    // Debounce DisplayChange — contrast theme switches fire multiple
    // WM_DISPLAYCHANGE messages with intermediate work areas.
    if matches!(win_event, WindowEvent::DisplayChange) {
        // Immediately clear inset cache and refresh high contrast state
        // (cheap operations that should happen right away).
        leopardwm_platform_win32::clear_inset_cache();
        {
            let mut state = ctx.state.lock().await;
            state.refresh_high_contrast();
            state.display_change_pending = true;
        }
        // Cancel pending timer and restart — only process after 500ms of quiet
        if let Some(handle) = ctx.display_change_timer.take() {
            handle.abort();
        }
        let dc_tx = ctx.event_tx.clone();
        *ctx.display_change_timer = Some(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let _ = dc_tx.send(DaemonEvent::DisplayChangeSettled).await;
        }));
        return;
    }
    // Handle MouseEnterWindow specially for focus-follows-mouse debouncing
    if let WindowEvent::MouseEnterWindow(hwnd) = win_event {
        let (enabled, delay_ms) = {
            let state = ctx.state.lock().await;
            (
                state.config.behavior.focus_follows_mouse,
                state.config.behavior.focus_follows_mouse_delay_ms,
            )
        };

        if enabled {
            // Cancel any pending focus timer
            if let Some(handle) = ctx.focus_follows_mouse_timer.take() {
                handle.abort();
            }

            // Schedule focus after delay (debouncing)
            let focus_tx = ctx.event_tx.clone();
            let delay = delay_ms;
            *ctx.focus_follows_mouse_timer = Some(tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay as u64))
                    .await;
                let _ = focus_tx
                    .send(DaemonEvent::FocusFollowsMouse { window_id: hwnd })
                    .await;
            }));
        }
    } else {
        {
            let mut state = ctx.state.lock().await;
            state.handle_window_event(win_event);

            // Start animation worker if the event triggered a transition.
            if state.is_animating() && !*ctx.animation_active {
                state.tick_animations(0);
                if let Ok(true) = state.send_animation_frame(ctx.animation_worker) {
                    *ctx.animation_active = true;
                    *ctx.last_frame_instant = Some(std::time::Instant::now());
                }
            }

            // Process drag hint overlay requests from event handler.
            // Drag ghost always shows regardless of snap_hints.enabled.
            if let Some(hint) = state.pending_drag_hint.take() {
                if let Some(ref overlay) = ctx.snap_hint_overlay {
                    match hint {
                        crate::state::DragHintAction::ShowGhost { rect } => {
                            overlay.show_snap_target(
                                leopardwm_core_layout::Rect::new(
                                    rect.x, rect.y, rect.width, rect.height,
                                ),
                            );
                        }
                        crate::state::DragHintAction::Hide => {
                            overlay.hide();
                        }
                    }
                }
            }

            // Update tray tooltip with current state
            if let Some(ref mgr) = ctx.tray_manager {
                let wc = state.all_managed_window_ids().len();
                let mc = state.monitors.len();
                let aws = (state.active_workspace_idx(state.focused_monitor) + 1) as u8;
                mgr.update_tooltip(
                    wc,
                    mc,
                    state.paused,
                    Some((ctx.hotkey_state.registered_count, ctx.hotkey_state.requested_count)),
                    aws,
                );
            }
            // Spawn vsync-aligned animation thread for resize preview transitions.
            if let Some(req) = state.pending_resize_animation.take() {
                if let Some(ref overlay) = ctx.snap_hint_overlay {
                    // Cancel any running preview animation thread.
                    state.resize_preview_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    state.resize_preview_cancel = cancel.clone();
                    let active = state.resize_animation_active.clone();
                    let overlay_hwnd = overlay.hwnd_raw();
                    std::thread::spawn(move || {
                        resize_preview_animation_loop(
                            overlay_hwnd,
                            req.start_rect,
                            req.target_rect,
                            cancel,
                            active,
                        );
                    });
                }
            }

            // Start animation if needed (e.g. animated snap-back)
            if state.is_animating() && !*ctx.animation_active {
                state.tick_animations(0);
                if let Ok(true) = state.send_animation_frame(ctx.animation_worker) {
                    *ctx.animation_active = true;
                    *ctx.last_frame_instant = Some(std::time::Instant::now());
                }
            }
        }
    }
}

/// Handle a global hotkey press; returns true when the daemon should shut down.
async fn handle_hotkey_event(
    ctx: &mut EventLoopCtx<'_>,
    hotkey_event: leopardwm_platform_win32::HotkeyEvent,
) -> bool {
    let mut requested_shutdown: Option<ShutdownMode> = None;
    let (should_animate, is_resize, column_rect, hint_duration) =
        if let Some(cmd) = ctx.hotkey_state.mapping.get(&hotkey_event.id).cloned() {
            debug!("Hotkey {} triggered, executing {:?}", hotkey_event.id, cmd);
            if let Some(mode) = shutdown_mode_for_command(&cmd) {
                requested_shutdown = Some(mode);
                (false, false, None, 200)
            } else {
                let is_resize = matches!(cmd, IpcCommand::Resize { .. });
                let mut state = ctx.state.lock().await;
                let response = state.handle_command(cmd);
                if let IpcResponse::Error { message } = response {
                    warn!("Hotkey command failed: {}", message);
                }
                let animating = state.is_animating();

                // Get column rect for snap hint if this is a resize
                let rect = if is_resize && state.config.snap_hints.enabled {
                    state.get_focused_column_rect()
                } else {
                    None
                };
                let duration = state.config.snap_hints.duration_ms;

                (animating, is_resize, rect, duration)
            }
        } else {
            warn!("Unknown hotkey ID: {}", hotkey_event.id);
            (false, false, None, 200)
        };

    if let Some(mode) = requested_shutdown {
        warn!(
            "Hotkey {} requested {}; running shutdown cleanup",
            hotkey_event.id,
            mode.label()
        );
        run_shutdown_cleanup(ctx.state, mode).await;
        return true;
    }

    // Show snap hint for resize operations
    if is_resize {
        if let (Some(ref overlay), Some(rect)) = (ctx.snap_hint_overlay, column_rect) {
            // Cancel any pending hide timer
            if let Some(handle) = ctx.snap_hint_timer_handle.take() {
                handle.abort();
            }

            // Show the snap hint
            overlay.show_snap_target(rect);

            // Schedule hide after duration
            let hide_tx = ctx.event_tx.clone();
            let duration = hint_duration;
            *ctx.snap_hint_timer_handle = Some(tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(duration as u64))
                    .await;
                let _ = hide_tx.send(DaemonEvent::HideSnapHint).await;
            }));
        }
    }

    // Start animation if needed
    if should_animate && !*ctx.animation_active {
        let mut state = ctx.state.lock().await;
        state.tick_animations(0);
        if let Ok(true) = state.send_animation_frame(ctx.animation_worker) {
            *ctx.animation_active = true;
            *ctx.last_frame_instant = Some(std::time::Instant::now());
        }
    }

    false
}

/// Handle a touchpad/scroll gesture; returns true when the daemon should shut down.
async fn handle_gesture_event(ctx: &mut EventLoopCtx<'_>, gesture_event: GestureEvent) -> bool {
    // Map gesture to command from config
    let gesture_config = {
        let state = ctx.state.lock().await;
        state.config.gestures.clone()
    };

    let cmd_str = match gesture_event {
        GestureEvent::SwipeLeft => &gesture_config.swipe_left,
        GestureEvent::SwipeRight => &gesture_config.swipe_right,
        GestureEvent::SwipeUp => &gesture_config.swipe_up,
        GestureEvent::SwipeDown => &gesture_config.swipe_down,
        GestureEvent::ScrollUp => &gesture_config.scroll_up,
        GestureEvent::ScrollDown => &gesture_config.scroll_down,
    };

    if let Some(cmd) = config::parse_command(cmd_str) {
        debug!("Gesture {:?} triggered, executing {:?}", gesture_event, cmd);
        if let Some(mode) = shutdown_mode_for_command(&cmd) {
            warn!(
                "Gesture {:?} requested {}; running shutdown cleanup",
                gesture_event,
                mode.label()
            );
            run_shutdown_cleanup(ctx.state, mode).await;
            return true;
        }
        {
            let mut state = ctx.state.lock().await;
            let response = state.handle_command(cmd);
            if let IpcResponse::Error { message } = response {
                warn!("Gesture command failed: {}", message);
            }
            if state.is_animating() && !*ctx.animation_active {
                state.tick_animations(0);
                if let Ok(true) = state.send_animation_frame(ctx.animation_worker) {
                    *ctx.animation_active = true;
                    *ctx.last_frame_instant = Some(std::time::Instant::now());
                }
            }
        }
    } else {
        warn!("Unknown command for gesture: {}", cmd_str);
    }

    false
}

/// Handle a tray menu event.
async fn handle_tray_event(ctx: &mut EventLoopCtx<'_>, tray_event: tray::TrayEvent) {
    match tray_event {
        tray::TrayEvent::Refresh => {
            info!("Tray: Refresh requested");
            let mut state = ctx.state.lock().await;
            let response = state.handle_command(IpcCommand::Refresh);
            if let IpcResponse::Error { message } = response {
                warn!("Refresh failed: {}", message);
            }
        }
        tray::TrayEvent::Reload => {
            info!("Tray: Reload config requested");
            let response = {
                let mut state = ctx.state.lock().await;
                state.handle_command(IpcCommand::Reload)
            };

            // If config was reloaded successfully, also reload hotkeys
            if matches!(response, IpcResponse::Ok) {
                reload_config_and_hotkeys(
                    ctx.state, ctx.hotkey_state, ctx.event_tx,
                    ctx.tray_manager, ctx.snap_hint_overlay,
                ).await;
                info!("Hotkeys reloaded after tray config reload");
            } else if let IpcResponse::Error { message } = response {
                warn!("Reload failed: {}", message);
            }
        }
        tray::TrayEvent::Exit => {
            info!("Tray: Exit requested");
            // Route tray exit through the unified shutdown path so all
            // cleanup (save_state + uncloak/reset) stays consistent.
            let _ = ctx.event_tx.send(DaemonEvent::Shutdown).await;
        }
        tray::TrayEvent::TogglePause => {
            let mut state = ctx.state.lock().await;
            if let Err(e) = state.toggle_pause("tray toggle") {
                warn!("Tray toggle pause failed: {}", e);
            }
            if let Some(ref mgr) = ctx.tray_manager {
                mgr.update_pause_text(state.paused);
                let wc = state.all_managed_window_ids().len();
                let mc = state.monitors.len();
                let aws = (state.active_workspace_idx(state.focused_monitor) + 1) as u8;
                mgr.update_tooltip(
                    wc,
                    mc,
                    state.paused,
                    Some((ctx.hotkey_state.registered_count, ctx.hotkey_state.requested_count)),
                    aws,
                );
            }
        }
        tray::TrayEvent::OpenConfig => {
            info!("Tray: Settings requested");
            let (config_snapshot, hc) = {
                let mut st = ctx.state.lock().await;
                st.refresh_high_contrast();
                (st.config.clone(), st.high_contrast)
            };
            *ctx.settings_handle = settings::SettingsWindowHandle::open(
                config_snapshot,
                ctx.settings_sync_tx.clone(),
                None,
                hc,
            );
        }
        tray::TrayEvent::OpenAbout => {
            info!("Tray: About requested");
            let (config_snapshot, hc) = {
                let mut st = ctx.state.lock().await;
                st.refresh_high_contrast();
                (st.config.clone(), st.high_contrast)
            };
            *ctx.settings_handle = settings::SettingsWindowHandle::open(
                config_snapshot,
                ctx.settings_sync_tx.clone(),
                Some("about"),
                hc,
            );
        }
        tray::TrayEvent::EditConfig => {
            info!("Tray: Edit config requested");
            let config_path = config::config_paths()
                .into_iter()
                .find(|p| p.exists())
                .or_else(|| config::config_paths().into_iter().next());
            if let Some(path) = config_path {
                let _ = std::process::Command::new("cmd")
                    .args(["/c", "start", "", &path.to_string_lossy()])
                    .spawn();
            }
        }
        tray::TrayEvent::ViewLogs => {
            info!("Tray: View logs requested");
            let log_dir = std::env::temp_dir();
            let _ = std::process::Command::new("cmd")
                .args(["/c", "start", "", &log_dir.to_string_lossy()])
                .spawn();
        }
        tray::TrayEvent::ReleaseAllWindows => {
            warn!("Tray: Release all windows requested");
            {
                let mut state = ctx.state.lock().await;
                // 1. Pause tiling
                if !state.paused {
                    let _ = state.toggle_pause("release all windows");
                }
                // 2. Clear focus and hide border
                state.hide_border();
                state.previous_focused_hwnd = None;
                let monitor = state.focused_monitor as i64;
                state.broadcast_focused_window_if_changed(monitor, None);
                // 3. Cascade all managed windows
                let window_ids = state.all_managed_window_ids();
                cascade_windows(&window_ids);
            }
            if let Some(ref mgr) = ctx.tray_manager {
                mgr.update_pause_text(true);
            }
            // 3. Ask user if they want to restart tiling
            let event_tx_clone = ctx.event_tx.clone();
            std::thread::spawn(move || {
                use windows::Win32::UI::WindowsAndMessaging::{
                    MessageBoxW, IDYES, MB_ICONQUESTION, MB_YESNO,
                };
                use windows::core::w;
                let result = unsafe {
                    MessageBoxW(
                        None,
                        w!("All windows have been released and cascaded.\n\nWould you like to restart tiling?"),
                        w!("LeopardWM"),
                        MB_YESNO | MB_ICONQUESTION,
                    )
                };
                if result == IDYES {
                    let _ = event_tx_clone.blocking_send(
                        DaemonEvent::Tray(tray::TrayEvent::TogglePause),
                    );
                }
            });
        }
        tray::TrayEvent::ToggleActiveBorder => {
            let mut state = ctx.state.lock().await;
            state.config.appearance.active_border =
                !state.config.appearance.active_border;
            let on = state.config.appearance.active_border;
            info!("Tray: Active border toggled to {}", on);
            if on {
                if let Some(hwnd) = state.previous_focused_hwnd {
                    state.show_border(hwnd);
                }
            } else {
                state.hide_border();
            }
            let _ = state.config.save();
        }
        tray::TrayEvent::ToggleFocusNewWindows => {
            let mut state = ctx.state.lock().await;
            state.config.behavior.focus_new_windows =
                !state.config.behavior.focus_new_windows;
            info!(
                "Tray: Focus new windows toggled to {}",
                state.config.behavior.focus_new_windows
            );
            let _ = state.config.save();
        }
        tray::TrayEvent::ToggleFocusFollowsMouse => {
            let mut state = ctx.state.lock().await;
            state.config.behavior.focus_follows_mouse =
                !state.config.behavior.focus_follows_mouse;
            info!(
                "Tray: Focus follows mouse toggled to {}",
                state.config.behavior.focus_follows_mouse
            );
            let _ = state.config.save();
        }
        tray::TrayEvent::ToggleAutoStart => {
            use leopardwm_platform_win32::autostart;
            let current = autostart::get_autostart().unwrap_or(false);
            let target = !current;
            let result = if target {
                std::env::current_exe()
                    .map_err(anyhow::Error::from)
                    .and_then(|exe| autostart::enable_autostart(&exe))
            } else {
                autostart::disable_autostart()
            };
            match result {
                Ok(()) => info!(
                    "Tray: Auto-start {}",
                    if target { "enabled" } else { "disabled" }
                ),
                Err(e) => warn!("Tray: failed to update auto-start: {}", e),
            }
            let state_guard = ctx.state.lock().await;
            sync_tray_toggles(ctx.tray_manager, &state_guard.config);
        }
        tray::TrayEvent::SetCenteringCenter => {
            let mut state = ctx.state.lock().await;
            if state.config.layout.centering_mode
                != config::CenteringModeConfig::Center
            {
                state.config.layout.centering_mode =
                    config::CenteringModeConfig::Center;
                info!("Tray: Centering mode set to Center");
                let cfg = state.config.clone();
                state.apply_config(cfg);
                if let Err(e) = state.apply_layout() {
                    warn!("Layout apply after centering change failed: {}", e);
                }
                let _ = state.config.save();
            }
            sync_tray_toggles(ctx.tray_manager, &state.config);
        }
        tray::TrayEvent::OpenReleasesPage => {
            info!("Tray: opening releases page");
            if let Err(e) = std::process::Command::new("cmd")
                .args(["/C", "start", "", update_check::RELEASES_PAGE_URL])
                .spawn()
            {
                warn!("Failed to open releases page: {}", e);
            }
        }
        tray::TrayEvent::SetCenteringJustInView => {
            let mut state = ctx.state.lock().await;
            if state.config.layout.centering_mode
                != config::CenteringModeConfig::JustInView
            {
                state.config.layout.centering_mode =
                    config::CenteringModeConfig::JustInView;
                info!("Tray: Centering mode set to JustInView");
                let cfg = state.config.clone();
                state.apply_config(cfg);
                if let Err(e) = state.apply_layout() {
                    warn!("Layout apply after centering change failed: {}", e);
                }
                let _ = state.config.save();
            }
            sync_tray_toggles(ctx.tray_manager, &state.config);
        }
        tray::TrayEvent::SetCenteringOnOverflow => {
            let mut state = ctx.state.lock().await;
            if state.config.layout.centering_mode
                != config::CenteringModeConfig::OnOverflow
            {
                state.config.layout.centering_mode =
                    config::CenteringModeConfig::OnOverflow;
                info!("Tray: Centering mode set to OnOverflow");
                let cfg = state.config.clone();
                state.apply_config(cfg);
                if let Err(e) = state.apply_layout() {
                    warn!("Layout apply after centering change failed: {}", e);
                }
                let _ = state.config.save();
            }
            sync_tray_toggles(ctx.tray_manager, &state.config);
        }
        tray::TrayEvent::SetPlacementNewColumn => {
            let mut state = ctx.state.lock().await;
            if state.config.behavior.new_window_placement
                != config::NewWindowPlacement::NewColumn
            {
                state.config.behavior.new_window_placement =
                    config::NewWindowPlacement::NewColumn;
                info!("Tray: New-window placement set to NewColumn");
                let _ = state.config.save();
            }
            sync_tray_toggles(ctx.tray_manager, &state.config);
        }
        tray::TrayEvent::SetPlacementInColumn => {
            let mut state = ctx.state.lock().await;
            if state.config.behavior.new_window_placement
                != config::NewWindowPlacement::InColumn
            {
                state.config.behavior.new_window_placement =
                    config::NewWindowPlacement::InColumn;
                info!("Tray: New-window placement set to InColumn");
                let _ = state.config.save();
            }
            sync_tray_toggles(ctx.tray_manager, &state.config);
        }
    }
}

/// Refresh tab-strip overlays so background icon-only changes stay fresh.
async fn handle_tab_strip_icon_poll(state: &Arc<Mutex<AppState>>) {
    // Single lock: check-and-refresh atomically so the state
    // we gated on (overlay present, workspace not fullscreen,
    // a Tabbed column exists, not paused) can't shift between
    // the check and the refresh call.
    let mut s = state.lock().await;
    let needs_refresh = !s.tab_strip_overlays.is_empty()
        && !s.paused
        && s.focused_workspace().is_some_and(|ws| {
            !ws.is_fullscreen()
                && (0..ws.column_count())
                    .any(|i| ws.column(i).is_some_and(|c| c.is_tabbed()))
        });
    if needs_refresh {
        s.update_tab_strip();
    }
}

/// Handle a tab-strip click action routed to the captured column identity.
async fn handle_tab_action(
    state: &Arc<Mutex<AppState>>,
    monitor: isize,
    workspace_idx: usize,
    column_idx: usize,
    tab_idx: usize,
    action: leopardwm_platform_win32::tab_strip::TabAction,
) {
    use leopardwm_platform_win32::tab_strip::TabAction;
    match action {
        TabAction::Activate => {
            // Trust the strip's captured identity over current
            // focus — focus may have changed between click and
            // dispatch, and we want the click to apply to the
            // column the user saw, not whatever is focused now.
            let mut s = state.lock().await;
            let needs_focus_switch = s.focused_monitor != monitor
                || s.active_workspace_idx(monitor) != workspace_idx
                || s.focused_workspace()
                    .is_none_or(|ws| ws.focused_column_index() != column_idx);
            if needs_focus_switch {
                s.focused_monitor = monitor;
                if let Some(ws) = s.focused_workspace_mut() {
                    let _ = ws.set_focus(column_idx, 0);
                }
            }
            let resp = s.handle_command(IpcCommand::SetActiveTab {
                column: column_idx,
                tab: tab_idx,
            });
            if let IpcResponse::Error { message } = resp {
                warn!("SetActiveTab from tab click failed: {}", message);
            }
        }
        TabAction::Close => {
            let s = state.lock().await;
            let target_hwnd = s
                .workspaces
                .get(&monitor)
                .and_then(|wss| wss.get(workspace_idx))
                .and_then(|ws| ws.column(column_idx))
                .and_then(|col| col.get(tab_idx));
            drop(s);
            match target_hwnd {
                Some(hwnd) => {
                    if let Err(e) = leopardwm_platform_win32::close_window(hwnd) {
                        warn!("close_window from tab strip failed: {}", e);
                    }
                }
                None => debug!(
                    "TabAction::Close target missing (monitor={}, ws={}, col={}, tab={})",
                    monitor, workspace_idx, column_idx, tab_idx
                ),
            }
        }
        TabAction::Untab => {
            let mut s = state.lock().await;
            if s.focused_monitor != monitor {
                s.focused_monitor = monitor;
            }
            s.pending_tab_focus = Some(crate::state::PendingTabFocus {
                monitor,
                workspace_idx,
                column_idx,
                tab_idx,
                set_at: std::time::Instant::now(),
            });
            // Run all workspace operations under one borrow,
            // collecting the proceed-or-abort decision so we
            // can drop the borrow before invoking handle_command.
            let proceed = match s.focused_workspace_mut() {
                None => false,
                Some(ws) => {
                    if ws.set_focus(column_idx, 0).is_err() {
                        false
                    } else if let Err(e) = ws.set_active_tab(column_idx, tab_idx) {
                        warn!("Untab set_active_tab failed: {}", e);
                        false
                    } else {
                        let col_len = ws
                            .column(column_idx)
                            .map_or(0, |c| c.windows().len());
                        col_len > 1
                    }
                }
            };
            if proceed {
                let resp = s.handle_command(IpcCommand::ExpelToRight);
                if let IpcResponse::Error { message } = resp {
                    warn!("ExpelToRight from untab failed: {}", message);
                    s.pending_tab_focus = None;
                }
            } else {
                s.pending_tab_focus = None;
            }
        }
        TabAction::Rename => {
            let s = state.lock().await;
            // Only one rename popup in flight at a time.
            if s.rename_dialog_active.swap(
                true,
                std::sync::atomic::Ordering::SeqCst,
            ) {
                debug!("Rename popup already active, ignoring duplicate request");
            } else {
                let initial = s.tab_title_for(
                    monitor,
                    workspace_idx,
                    column_idx,
                    tab_idx,
                );
                // Resolve the strip overlay that owns this
                // column so the rename popup lands precisely
                // over the right tab cell. With per-column
                // overlays the lookup is keyed by identity.
                let (tab_rect, colors) = match s
                    .tab_strip_overlays
                    .get(&(monitor, workspace_idx, column_idx))
                {
                    Some(strip) => (
                        strip.tab_screen_rect(tab_idx),
                        strip.current_colors(),
                    ),
                    None => (None, None),
                };
                // Resolve the tab's HWND ONCE at spawn —
                // this is the authoritative rename target.
                // Capturing the HWND here means a column
                // mutation (close/reorder/untab) while the
                // popup is open won't retarget the override
                // to a different window. Index-based
                // resolution at submission time was racy.
                let target_hwnd_opt = s
                    .workspaces
                    .get(&monitor)
                    .and_then(|wss| wss.get(workspace_idx))
                    .and_then(|ws| ws.column(column_idx))
                    .and_then(|col| col.get(tab_idx));
                let icon = target_hwnd_opt
                    .and_then(leopardwm_platform_win32::get_window_icon);
                let tx = s.rename_result_tx.clone();
                let guard = s.rename_dialog_active.clone();
                // Second clone — needed for the spawn-failure
                // path below since the closure consumes `guard`.
                let guard_for_spawn_failure = guard.clone();
                drop(s);
                match (tab_rect, colors, target_hwnd_opt) {
                    (Some(rect), Some(c), Some(target_hwnd)) => {
                        let spawn_res = std::thread::Builder::new()
                            .name("tab-rename-popup".into())
                            .spawn(move || {
                                let result =
                                    leopardwm_platform_win32::dialog::show_rename_inline_popup(
                                        initial,
                                        rect,
                                        c.active_bg,
                                        c.active_text,
                                        icon,
                                    )
                                    .unwrap_or_else(|e| {
                                        warn!("Rename popup failed to open: {}", e);
                                        None
                                    });
                                if let Some(sender) = tx {
                                    let _ = sender.send(
                                        crate::events::TabRenameResult {
                                            monitor,
                                            workspace_idx,
                                            column_idx,
                                            tab_idx,
                                            target_hwnd,
                                            new_title: result,
                                        },
                                    );
                                }
                                guard.store(false, std::sync::atomic::Ordering::SeqCst);
                            });
                        // Thread::spawn can fail (OOM, hit
                        // OS thread limit). The closure owns
                        // `guard`, so on Err we must reset
                        // it ourselves — otherwise the
                        // active-flag stays `true` forever
                        // and all future renames are
                        // silently suppressed.
                        if let Err(e) = spawn_res {
                            warn!("Failed to spawn rename popup thread: {}", e);
                            guard_for_spawn_failure
                                .store(false, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                    _ => {
                        debug!(
                            "TabAction::Rename: strip not visible, tab idx out of range, or HWND missing"
                        );
                        guard.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            }
        }
    }
}

/// Apply a rename-dialog result as a tab title override.
async fn handle_tab_rename_submitted(
    state: &Arc<Mutex<AppState>>,
    monitor: isize,
    workspace_idx: usize,
    column_idx: usize,
    tab_idx: usize,
    target_hwnd: u64,
    new_title: Option<String>,
) {
    let Some(title) = new_title else {
        // User cancelled; nothing to do.
        return;
    };
    let mut s = state.lock().await;
    // Use the HWND captured at spawn time — column mutation
    // during the popup's lifetime would otherwise retarget
    // the override to a different window.
    let hwnd = target_hwnd;
    if !leopardwm_platform_win32::is_window_valid(hwnd) {
        debug!(
            "TabRenameSubmitted: target window gone (monitor={}, ws={}, col={}, tab={}, hwnd={})",
            monitor, workspace_idx, column_idx, tab_idx, hwnd
        );
        return;
    }
    if title.is_empty() {
        s.tab_title_overrides.remove(&hwnd);
    } else if title.len()
        > leopardwm_platform_win32::dialog::TAB_TITLE_MAX_BYTES
    {
        // The dialog already rejects over-length input, but
        // belt-and-suspenders in case a future surface bypasses it.
        warn!(
            "Rejecting tab title override ({} bytes > {} max)",
            title.len(),
            leopardwm_platform_win32::dialog::TAB_TITLE_MAX_BYTES
        );
        return;
    } else {
        s.tab_title_overrides.insert(hwnd, title);
    }
    // Refresh the strip so the new label shows immediately.
    s.update_tab_strip();
    let _ = s.save_state();
}

/// Handle a settings-window event.
async fn handle_settings_event(
    ctx: &mut EventLoopCtx<'_>,
    settings_event: settings::SettingsEvent,
) {
    match settings_event {
        settings::SettingsEvent::Saved => {
            info!("Settings: config saved, triggering reload");
            let response = {
                let mut state = ctx.state.lock().await;
                state.handle_command(IpcCommand::Reload)
            };
            if matches!(response, IpcResponse::Ok) {
                reload_config_and_hotkeys(
                    ctx.state, ctx.hotkey_state, ctx.event_tx,
                    ctx.tray_manager, ctx.snap_hint_overlay,
                ).await;
                info!("Hotkeys reloaded after settings save");
            } else if let IpcResponse::Error { message } = response {
                warn!("Reload after settings save failed: {}", message);
            }
        }
    }
}

/// Process an applied animation frame: feed back violations, tick, and land the final layout.
async fn handle_animation_frame_applied(
    ctx: &mut EventLoopCtx<'_>,
    frame_result: animation_worker::FrameResult,
) {
    {
        let mut state = ctx.state.lock().await;
        state.applying_layout = false;
        // Feed width violations back to the layout engine so it can
        // allocate correct column widths on subsequent frames.
        if !frame_result.width_violations.is_empty() {
            for violation in &frame_result.width_violations {
                // Skip violations where min_width >= viewport width —
                // the window is temporarily fullscreen/maximized by
                // the app, not enforcing a genuine minimum.
                let vw = state.find_window_workspace(violation.window_id)
                    .map(|(mid, _)| state.viewport_width_for(mid))
                    .unwrap_or(i32::MAX);
                if violation.min_width >= vw {
                    debug!(
                        "Ignoring viewport-sized width violation for window {} ({}px >= {}px viewport)",
                        violation.window_id, violation.min_width, vw
                    );
                    continue;
                }
                for ws_vec in state.workspaces.values_mut() {
                    for ws in ws_vec.iter_mut() {
                        if ws.contains_window(violation.window_id) {
                            ws.set_window_min_width(
                                violation.window_id,
                                violation.min_width,
                            );
                        }
                    }
                }
            }
            // Adjust stored column widths so total_width, scroll
            // clamping, and focus-visible all account for min-widths.
            let mut widths_changed = false;
            for ws_vec in state.workspaces.values_mut() {
                for ws in ws_vec.iter_mut() {
                    if ws.apply_min_width_constraints() {
                        widths_changed = true;
                    }
                }
            }
            // When column widths change due to min-width constraints,
            // the active scroll animation's target (computed with old
            // widths) may no longer align with actual column positions.
            // Re-validate the scroll target for each active workspace.
            if widths_changed {
                let monitor_ids: Vec<_> = state.workspaces.keys().cloned().collect();
                for monitor_id in monitor_ids {
                    let idx = state.active_workspace_idx(monitor_id);
                    let vw = state.monitors.get(&monitor_id).map(|m| m.work_area.width);
                    if let (Some(ws), Some(vw)) = (
                        state.workspaces.get_mut(&monitor_id).and_then(|v| v.get_mut(idx)),
                        vw,
                    ) {
                        if ws.is_animating() {
                            ws.ensure_focused_visible_animated(vw);
                        }
                    }
                }
            }
        }
        // Feed height violations back to the layout engine so it
        // can allocate correct intra-column heights on subsequent
        // frames.
        if !frame_result.height_violations.is_empty() {
            for violation in &frame_result.height_violations {
                // Skip violations where min_height >= viewport
                // height — the window is temporarily fullscreen
                // /maximized by the app, not enforcing a genuine
                // minimum that large.
                let vh = state.find_window_workspace(violation.window_id)
                    .and_then(|(mid, _)| state.monitors.get(&mid).map(|m| m.work_area.height))
                    .unwrap_or(i32::MAX);
                if violation.min_height >= vh {
                    debug!(
                        "Ignoring viewport-sized height violation for window {} ({}px >= {}px viewport)",
                        violation.window_id, violation.min_height, vh
                    );
                    continue;
                }
                for ws_vec in state.workspaces.values_mut() {
                    for ws in ws_vec.iter_mut() {
                        if ws.contains_window(violation.window_id) {
                            ws.set_window_min_height(
                                violation.window_id,
                                violation.min_height,
                            );
                        }
                    }
                }
            }
        }
    }
    if let Err(ref e) = frame_result.apply_result {
        warn!("Animation frame failed: {}", e);
    }
    // Measure real elapsed time (cap at 100ms to prevent jump from stalls)
    let delta_ms = ctx.last_frame_instant
        .map(|t| t.elapsed().as_millis().min(100) as u64)
        .unwrap_or(16);
    *ctx.last_frame_instant = Some(std::time::Instant::now());

    let still_animating = {
        let mut state = ctx.state.lock().await;
        let running = state.tick_animations(delta_ms);
        // Reposition the border AFTER the tick so it reflects the
        // SAME interpolated state the next animation frame will
        // position windows at. The border SetWindowPos and the
        // worker's window SetWindowPos calls below both commit
        // before the next DwmFlush vsync, so the border arrives
        // on screen in the same frame as the windows. Doing
        // show_border BEFORE tick (the previous order) lagged the
        // border by one vsync — windows updated at vsync N,
        // border at vsync N+1 — which is what the user noticed
        // as "border scrolling at a different framerate".
        if let Some(hwnd) = state.previous_focused_hwnd {
            if state.config.appearance.active_border {
                state.show_border(hwnd);
            }
        }
        if running || state.is_animating() {
            matches!(
                state.send_animation_frame(ctx.animation_worker),
                Ok(true)
            )
        } else {
            false
        }
    };
    if !still_animating {
        *ctx.animation_active = false;
        *ctx.last_frame_instant = None;
        // Final landing pass: apply exact resting positions.
        // The last animation frame was at an interpolated offset
        // slightly before the target; this pass uses the exact
        // scroll_offset (set to target by tick_animation) and
        // bypasses the worker cache to reposition every window.
        {
            let mut state = ctx.state.lock().await;

            // HWND-recycling guard for ghosted wids: if the
            // source class changed since registration, the HWND
            // was recycled or the window died — drop the entry
            // (GhostEntry::Drop unregisters) without uncloaking.
            let ghost_wids: Vec<u64> =
                state.ghost_handles.keys().copied().collect();
            let mut surviving: Vec<u64> = Vec::with_capacity(ghost_wids.len());
            for wid in ghost_wids {
                let still_valid = {
                    let class_now =
                        leopardwm_platform_win32::thumbnail::class_name(wid);
                    state
                        .ghost_handles
                        .get(&wid)
                        .map(|e| e.class_at_register == class_now)
                        .unwrap_or(false)
                };
                if still_valid {
                    surviving.push(wid);
                } else {
                    leopardwm_platform_win32::unmark_ghost_cloaked(wid);
                    state.ghost_handles.remove(&wid);
                    // Don't apply_cloak_state — we don't know
                    // what HWND we'd be uncloaking.
                }
            }

            // Uncloak surviving sources BEFORE apply_layout so
            // the synchronous SetWindowPos hits a visible HWND.
            for &wid in &surviving {
                leopardwm_platform_win32::unmark_ghost_cloaked(wid);
                leopardwm_platform_win32::apply_cloak_state(wid);
            }

            // Only this landing pass follows an async frame burst,
            // so it is the only `apply_layout` that needs to fire
            // the sticky-compositor `(w-1 → w)` nudge. Routine
            // applies (focus shifts within view, event refreshes,
            // drag finalizations) skip the nudge to avoid a
            // visible 1 px wobble on every Chromium / Firefox
            // window every time the layout is re-applied.
            state.post_animation_nudge_pending = true;
            let landing_ok = state.apply_layout().is_ok();
            if !landing_ok {
                warn!(
                    "Final landing layout failed; dropping any active ghosts \
                     without crossfade"
                );
            }

            // Stage D: if landing succeeded and we have ghosts,
            // transfer their handles to the worker for an
            // 8-frame ease-in-cubic crossfade. If landing
            // failed, drop handles immediately (hard cut) —
            // a fade over a misaligned source produces a
            // visible duplicate.
            if landing_ok && !surviving.is_empty() {
                state.crossfade_epoch_counter =
                    state.crossfade_epoch_counter.saturating_add(1);
                let epoch = state.crossfade_epoch_counter;
                let mut entries: Vec<animation_worker::CrossfadeEntry> =
                    Vec::with_capacity(surviving.len());
                let mut sources: std::collections::HashSet<u64> =
                    std::collections::HashSet::with_capacity(surviving.len());
                for wid in &surviving {
                    if let Some(entry) = state.ghost_handles.remove(wid) {
                        let dest = entry.final_dest_client_rect;
                        entries.push(animation_worker::CrossfadeEntry {
                            handle_isize: entry.take_isize(),
                            dest_client_rect: dest,
                        });
                        sources.insert(*wid);
                    }
                }
                // Any non-surviving entries (HWND recycled) are
                // already cleared above. Drop any stragglers
                // belt-and-suspenders.
                state.ghost_handles.clear();
                let entry_count = entries.len();
                state
                    .crossfade_sources
                    .insert(epoch, (sources, std::time::Instant::now()));
                state.active_crossfade =
                    Some(crate::state::CrossfadeState { epoch });
                debug!(
                    "ghost: crossfade epoch {} dispatched for {} source(s)",
                    epoch, entry_count
                );
                if let Err(e) =
                    ctx.animation_worker.send_crossfade(epoch, entries, 8)
                {
                    warn!("Failed to send crossfade to worker: {}", e);
                    // No worker: clear state so next transition
                    // isn't stuck waiting for a CrossfadeComplete
                    // that will never arrive.
                    state.active_crossfade = None;
                    state.crossfade_sources.remove(&epoch);
                }
            } else {
                // Hard cut: drop handles immediately. Each
                // GhostEntry::Drop calls unregister_raw.
                state.ghost_handles.clear();
            }

            // Re-sync OS foreground to match the workspace's focus.
            // During animation, SetWindowPos can trigger spurious
            // EVENT_SYSTEM_FOREGROUND events that override
            // focused_column. This re-asserts the correct focus
            // after the animation has settled.
            let pending_sticky = state.pending_sticky_refocus.take();
            if !state.paused {
                state.sync_foreground_window();
                // A workspace switch left a focused pinned window behind
                // it: those same spurious foreground events can have
                // clobbered previous_focused_hwnd mid-slide, making the
                // sync above land on the destination's tiled focus.
                // Re-assert the pinned window's focus.
                if let Some(wid) = pending_sticky {
                    state.refocus_sticky_window(wid);
                }
            }
        }
        debug!("All animations complete");
    }
}

/// Apply debounced focus-follows-mouse focus.
async fn handle_focus_follows_mouse(ctx: &mut EventLoopCtx<'_>, window_id: u64) {
    let mut state = ctx.state.lock().await;
    if state.config.behavior.focus_follows_mouse {
        let applied = state.apply_focus_follows_mouse(window_id);
        if applied && state.is_animating() && !*ctx.animation_active {
            state.tick_animations(0);
            if let Ok(true) = state.send_animation_frame(ctx.animation_worker) {
                *ctx.animation_active = true;
                *ctx.last_frame_instant = Some(std::time::Instant::now());
            }
        }
    }
}

/// Process a debounced display change with the settled monitor state.
async fn handle_display_change_settled(ctx: &mut EventLoopCtx<'_>) {
    // Debounced display change — process the final monitor state after
    // WM_DISPLAYCHANGE messages have stopped (theme/DPI transitions settled).
    // Clear inset cache again — MovedOrResized events during the transition
    // may have re-populated it with stale border metrics.
    leopardwm_platform_win32::clear_inset_cache();
    ctx.animation_worker.clear_cache();
    // Resize the DWM thumbnail host to the new virtual-screen
    // geometry so subsequent ghost-animation registrations use
    // correct coordinates. Safe no-op when host construction
    // failed earlier (returns Ok with hwnd_raw == 0).
    leopardwm_platform_win32::thumbnail::host().resize_to_virtual_screen();
    let mut state = ctx.state.lock().await;
    // Cancel any in-flight ghost animation — its rects were
    // computed under the old geometry.
    state.abort_active_ghost_transition();
    state.display_change_pending = false;
    // Clear stale min-width and min-height constraints — they were
    // computed with old border metrics and cause cumulative column
    // shrinking / wrong intra-column distribution on theme changes.
    for ws_vec in state.workspaces.values_mut() {
        for ws in ws_vec.iter_mut() {
            ws.clear_all_min_widths();
            ws.clear_all_min_heights();
        }
    }
    state.handle_window_event(WindowEvent::DisplayChange);
    state.refresh_reduce_motion();

    if state.is_animating() && !*ctx.animation_active {
        state.tick_animations(0);
        if let Ok(true) = state.send_animation_frame(ctx.animation_worker) {
            *ctx.animation_active = true;
            *ctx.last_frame_instant = Some(std::time::Instant::now());
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse command-line arguments
    let args = Args::parse();

    let (config, config_warnings) = bootstrap_config()?;

    // Install panic hook to uncloak all windows and write a crash report
    install_panic_hook();

    info!("LeopardWM daemon starting...");
    info!("Version: {}", env!("CARGO_PKG_VERSION"));

    // Check if another instance is already running
    let ipc_pipe_names = pipe_name_candidates();
    if check_already_running().await {
        eprintln!("Error: Another leopardwm-daemon instance is already running.");
        eprintln!("Use 'leopardwm-cli status' to check the running instance.");
        error!(
            "Another leopardwm-daemon instance is already running (active pipe candidates: {})",
            ipc_pipe_names.join(", ")
        );
        std::process::exit(1);
    }

    info!(
        "Configuration loaded: gap={}, outer_gaps=[{},{},{},{}], width_presets={:?}, log_level={}",
        config.layout.gap,
        config.layout.outer_gap_left,
        config.layout.outer_gap_right,
        config.layout.outer_gap_top,
        config.layout.outer_gap_bottom,
        config.layout.width_presets,
        config.behavior.log_level
    );

    // Detect all monitors
    let monitors = detect_monitors();

    // Initialize state with config and monitors
    #[allow(clippy::arc_with_non_send_sync)]
    let state = Arc::new(Mutex::new(AppState::new_with_config(
        config.clone(),
        monitors,
    )));

    // Enumerate existing windows
    info!("Enumerating windows...");
    {
        let mut state = state.lock().await;
        init_workspace_state(&mut state);
    }

    // Create event channel
    let (event_tx, mut event_rx) = mpsc::channel::<DaemonEvent>(100);

    // Collect forwarding thread handles for graceful shutdown
    let mut thread_handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    spawn_tab_forwarders(&state, &event_tx, &mut thread_handles).await;

    // Install WinEvent hooks for window lifecycle tracking (if enabled in config)
    let _hook_handle = setup_window_hooks(&config, &event_tx, &mut thread_handles);

    // Register global hotkeys (mutable to support reload)
    let mut hotkey_state = if args.skip_hotkeys() {
        info!("Hotkeys disabled by command-line flag");
        HotkeyState {
            handle: None,
            mapping: HashMap::new(),
            requested_count: 0,
            registered_count: 0,
        }
    } else {
        setup_hotkeys(&config, event_tx.clone())
    };

    // Install mouse hook for focus-follows-mouse (if enabled)
    let _mouse_hook_handle = setup_mouse_hook(&config, &event_tx, &mut thread_handles);

    // Register gesture detection (if enabled)
    let _gesture_handle = setup_gestures(&config, &event_tx, &mut thread_handles);

    // Initialize overlay for snap hints and drag ghost preview.
    // Always created — snap_hints.enabled only gates resize-hint visibility,
    // drag ghost preview always works regardless.
    let snap_hint_overlay: Option<OverlayWindow> = match OverlayWindow::new() {
        Ok(overlay) => {
            overlay.set_opacity(config.snap_hints.opacity);
            info!("Overlay initialized (snap hints {}, opacity {})", if config.snap_hints.enabled { "enabled" } else { "disabled" }, config.snap_hints.opacity);
            Some(overlay)
        }
        Err(e) => {
            warn!("Failed to create overlay: {}. Snap hints and drag ghost disabled.", e);
            None
        }
    };

    // Initialize system tray icon
    let tray_manager = setup_tray(&config, &event_tx, &mut thread_handles);

    // Update checker — daily GitHub Releases poll, opt-out via behavior.check_for_updates.
    let update_check_cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if config.behavior.check_for_updates {
        let tx = event_tx.clone();
        let cancel = update_check_cancel.clone();
        update_check::spawn_update_checker(cancel, move |tag| {
            // Best-effort send — if the receiver is gone we're already shutting down.
            let _ = tx.blocking_send(DaemonEvent::UpdateAvailable(tag));
        });
    }

    // Tab-strip icon poll: Windows has no MSAA event for icon-only
    // changes (e.g., Discord/Slack swapping in a notification-badge
    // icon without touching the window title), so a low-frequency tick
    // is the pragmatic way to keep background tab icons fresh. 2s is
    // slow enough that the periodic GDI work is negligible and fast
    // enough that badge updates feel responsive.
    {
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(2000));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if tx.send(DaemonEvent::TabStripIconPoll).await.is_err() {
                    break;
                }
            }
        });
    }

    // Settings window forwarding channel + handle
    let (settings_sync_tx, settings_sync_rx) = std::sync::mpsc::channel();
    match spawn_forwarding_thread(
        "settings-fwd",
        settings_sync_rx,
        event_tx.clone(),
        DaemonEvent::Settings,
    ) {
        Ok(handle) => thread_handles.push(handle),
        Err(e) => warn!("{}", e),
    }
    let mut _settings_handle: Option<settings::SettingsWindowHandle> = None;

    // Spawn IPC server. Subscribe is handled via a dedicated
    // DaemonEvent variant routed through this channel, so the per-client
    // task itself doesn't need direct AppState access.
    let ipc_tx = event_tx.clone();
    tokio::spawn(async move {
        run_ipc_server(ipc_tx).await;
    });

    info!("IPC server listening on {}", preferred_pipe_name());

    // Install Ctrl+C handler so terminal kill triggers graceful shutdown
    {
        let shutdown_tx = event_tx.clone();
        tokio::spawn(async move {
            if let Ok(()) = tokio::signal::ctrl_c().await {
                info!("Ctrl+C received, initiating shutdown...");
                let _ = shutdown_tx.send(DaemonEvent::Shutdown).await;
            }
        });
    }

    // Print startup banner for immediate user feedback
    print_banner(&state, &config_warnings, &hotkey_state, &args).await;

    // Apply initial layout so windows are tiled on startup
    {
        let mut state = state.lock().await;
        if let Err(e) = state.apply_layout() {
            warn!("Failed to apply initial layout: {}", e);
        }
        // Set the DWM active border color on the focused window immediately
        state.sync_foreground_window();
    }

    info!("Ready. Use leopardwm-cli to send commands.");

    // Persistent animation worker thread (DwmFlush-based vsync pacing)
    let animation_worker = animation_worker::AnimationWorkerHandle::spawn(event_tx.clone())
        .expect("Failed to spawn animation worker");
    {
        let mut state_guard = state.lock().await;
        state_guard.animation_worker_control = Some(animation_worker.control());
    }
    let mut animation_active = false;
    let mut last_frame_instant: Option<std::time::Instant> = None;

    // Snap hint timer handle - cancels pending hide operation when new hint is shown
    let mut snap_hint_timer_handle: Option<tokio::task::JoinHandle<()>> = None;

    // Focus-follows-mouse timer handle - debounces rapid mouse movements
    let mut focus_follows_mouse_timer: Option<tokio::task::JoinHandle<()>> = None;

    // Display change debounce timer — contrast theme switches fire multiple
    // WM_DISPLAYCHANGE messages with intermediate work areas. We delay
    // processing until the changes settle to avoid sizing windows to a
    // transient work area.
    let mut display_change_timer: Option<tokio::task::JoinHandle<()>> = None;

    let mut ctx = EventLoopCtx {
        state: &state,
        event_tx: &event_tx,
        hotkey_state: &mut hotkey_state,
        tray_manager: &tray_manager,
        snap_hint_overlay: &snap_hint_overlay,
        settings_sync_tx: &settings_sync_tx,
        settings_handle: &mut _settings_handle,
        animation_worker: &animation_worker,
        animation_active: &mut animation_active,
        last_frame_instant: &mut last_frame_instant,
        snap_hint_timer_handle: &mut snap_hint_timer_handle,
        focus_follows_mouse_timer: &mut focus_follows_mouse_timer,
        display_change_timer: &mut display_change_timer,
    };

    // Main event loop
    loop {
        let event = match event_rx.recv().await {
            Some(e) => e,
            None => break,
        };

        match event {
            DaemonEvent::IpcSubscribe { events, responder } => {
                handle_ipc_subscribe(&state, events, responder).await;
            }
            DaemonEvent::IpcCommand { cmd, responder } => {
                if handle_ipc_command(&mut ctx, cmd, responder).await {
                    break;
                }
            }
            DaemonEvent::WindowEvent(win_event) => {
                process_window_event(&mut ctx, win_event).await;
            }
            DaemonEvent::Hotkey(hotkey_event) => {
                if handle_hotkey_event(&mut ctx, hotkey_event).await {
                    break;
                }
            }
            DaemonEvent::Gesture(gesture_event) => {
                if handle_gesture_event(&mut ctx, gesture_event).await {
                    break;
                }
            }
            DaemonEvent::Tray(tray_event) => {
                handle_tray_event(&mut ctx, tray_event).await;
            }
            DaemonEvent::UpdateAvailable(tag) => {
                if let Some(ref mgr) = tray_manager {
                    mgr.set_available_update(Some(tag));
                }
            }
            DaemonEvent::TabStripIconPoll => {
                handle_tab_strip_icon_poll(&state).await;
            }
            DaemonEvent::TabAction {
                monitor,
                workspace_idx,
                column_idx,
                tab_idx,
                action,
            } => {
                handle_tab_action(&state, monitor, workspace_idx, column_idx, tab_idx, action)
                    .await;
            }
            DaemonEvent::TabRenameSubmitted {
                monitor,
                workspace_idx,
                column_idx,
                tab_idx,
                target_hwnd,
                new_title,
            } => {
                handle_tab_rename_submitted(
                    &state,
                    monitor,
                    workspace_idx,
                    column_idx,
                    tab_idx,
                    target_hwnd,
                    new_title,
                )
                .await;
            }
            DaemonEvent::Settings(settings_event) => {
                handle_settings_event(&mut ctx, settings_event).await;
            }
            DaemonEvent::AnimationFrameApplied(frame_result) => {
                handle_animation_frame_applied(&mut ctx, frame_result).await;
            }
            DaemonEvent::CrossfadeComplete { epoch } => {
                let mut state = state.lock().await;
                let active = state.active_crossfade.as_ref().map(|s| s.epoch);
                if active == Some(epoch) {
                    state.active_crossfade = None;
                }
                // Always release this epoch's re-registration barrier.
                // Per-epoch tracking means an aborted fade's stale
                // CrossfadeComplete only clears its own entry, not a
                // newer in-flight fade's.
                state.crossfade_sources.remove(&epoch);
            }
            DaemonEvent::HideSnapHint => {
                if let Some(ref overlay) = snap_hint_overlay {
                    overlay.hide();
                    debug!("Snap hint hidden");
                }
            }
            DaemonEvent::FocusFollowsMouse { window_id } => {
                handle_focus_follows_mouse(&mut ctx, window_id).await;
            }
            DaemonEvent::PowerStateChanged { on_battery_or_saver } => {
                let mut state = state.lock().await;
                state.on_battery_or_saver = on_battery_or_saver;
                state.refresh_reduce_motion();
            }
            DaemonEvent::DisplayChangeSettled => {
                handle_display_change_settled(&mut ctx).await;
            }
            DaemonEvent::Shutdown => {
                info!("Shutdown signal received");
                run_shutdown_cleanup(&state, ShutdownMode::Graceful).await;
                break;
            }
        }
    }

    // Stop the update-checker worker so it doesn't hold up shutdown.
    update_check_cancel.store(true, std::sync::atomic::Ordering::SeqCst);

    // Clean up animation worker (Drop sends Shutdown and joins)
    drop(animation_worker);

    // Clean up timers if running
    if let Some(handle) = snap_hint_timer_handle {
        handle.abort();
    }
    if let Some(handle) = focus_follows_mouse_timer {
        handle.abort();
    }
    if let Some(handle) = display_change_timer {
        handle.abort();
    }

    // Join forwarding threads with timeout for graceful shutdown
    info!("Waiting for forwarding threads to exit...");
    let shutdown_deadline = std::time::Instant::now() + Duration::from_secs(5);
    for handle in thread_handles {
        let remaining = shutdown_deadline.saturating_duration_since(std::time::Instant::now());
        let per_thread = remaining.min(Duration::from_secs(3));
        let mut handle = Some(handle);
        if !join_with_timeout(&mut handle, per_thread) {
            warn!("A forwarding thread did not exit within timeout, continuing shutdown");
        }
    }

    info!("LeopardWM daemon shutting down.");
    Ok(())
}
