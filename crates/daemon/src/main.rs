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
mod settings;
mod startup;
mod state;
#[cfg(test)]
mod tests;
mod tray;

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
    set_dpi_awareness, uncloak_all_visible_windows, GestureEvent, Hotkey, HotkeyId,
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
/// Sync tray quick-toggle check marks with the current config.
fn sync_tray_toggles(tray_manager: &Option<tray::TrayManager>, config: &Config) {
    if let Some(ref mgr) = tray_manager {
        mgr.update_quick_toggles(
            config.appearance.active_border,
            config.behavior.focus_new_windows,
            config.behavior.focus_follows_mouse,
            match config.layout.centering_mode {
                config::CenteringModeConfig::Center => tray::CENTERING_CENTER,
                config::CenteringModeConfig::JustInView => tray::CENTERING_JUST_IN_VIEW,
            },
        );
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

    // Build hotkey definitions and command mapping
    let mut hotkeys = Vec::new();
    let mut mapping = HashMap::new();
    let mut next_id: HotkeyId = 1;

    for (key_str, cmd_str) in config_hotkeys {
        if let Some((modifiers, vk)) = parse_hotkey_string(key_str) {
            if let Some(cmd) = config::parse_command(cmd_str) {
                hotkeys.push(Hotkey::new(next_id, modifiers, vk));
                mapping.insert(next_id, cmd);
                debug!(
                    "Configured hotkey {}: {} -> {:?}",
                    next_id, key_str, cmd_str
                );
                next_id += 1;
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

#[tokio::main]
async fn main() -> Result<()> {
    // Parse command-line arguments
    let args = Args::parse();

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

    // Install panic hook to uncloak all windows and write a crash report
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("[leopardwm] PANIC detected — emergency uncloaking all windows");
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
    let monitors = match enumerate_monitors() {
        Ok(monitors) if !monitors.is_empty() => {
            info!("Detected {} monitor(s):", monitors.len());
            for m in &monitors {
                info!(
                    "  Monitor {}: {}x{} (work area: {}x{} at {},{}){} \"{}\"",
                    m.id,
                    m.rect.width,
                    m.rect.height,
                    m.work_area.width,
                    m.work_area.height,
                    m.work_area.x,
                    m.work_area.y,
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
            }]
        }
    };

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

    // Create event channel
    let (event_tx, mut event_rx) = mpsc::channel::<DaemonEvent>(100);

    // Collect forwarding thread handles for graceful shutdown
    let mut thread_handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    // Install WinEvent hooks for window lifecycle tracking (if enabled in config)
    let _hook_handle = if config.behavior.track_focus_changes {
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
    let _mouse_hook_handle = if config.behavior.focus_follows_mouse {
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
    };

    // Register gesture detection (if enabled)
    let _gesture_handle = if config.gestures.enabled {
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
    };

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
    // Create an intermediate sync channel that bridges tray events to the async event loop
    let tray_manager = {
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

        let initial_toggles = tray::QuickToggleState {
            active_border: config.appearance.active_border,
            focus_new_windows: config.behavior.focus_new_windows,
            focus_follows_mouse: config.behavior.focus_follows_mouse,
            centering_mode: match config.layout.centering_mode {
                config::CenteringModeConfig::Center => tray::CENTERING_CENTER,
                config::CenteringModeConfig::JustInView => tray::CENTERING_JUST_IN_VIEW,
            },
        };
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
    };

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

    // Spawn IPC server
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
    {
        let state = state.lock().await;
        let monitor_names: Vec<String> = state
            .monitors
            .values()
            .map(|m| m.device_name.clone())
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
        });
    }

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
    let mut animation_active = false;
    let mut last_frame_instant: Option<std::time::Instant> = None;

    // Snap hint timer handle - cancels pending hide operation when new hint is shown
    let mut snap_hint_timer_handle: Option<tokio::task::JoinHandle<()>> = None;

    // Focus-follows-mouse timer handle - debounces rapid mouse movements
    let mut focus_follows_mouse_timer: Option<tokio::task::JoinHandle<()>> = None;

    // Main event loop
    loop {
        let event = match event_rx.recv().await {
            Some(e) => e,
            None => break,
        };

        match event {
            DaemonEvent::IpcCommand { cmd, responder } => {
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
                    run_shutdown_cleanup(&state, mode).await;
                    break;
                }

                let is_reload = matches!(&cmd, IpcCommand::Reload);
                let is_resize = matches!(&cmd, IpcCommand::Resize { .. });
                let is_toggle_pause = matches!(&cmd, IpcCommand::TogglePause);

                let (response, should_animate, column_rect, hint_duration) = {
                    let mut state = state.lock().await;
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
                        &state, &mut hotkey_state, &event_tx,
                        &tray_manager, &snap_hint_overlay,
                    ).await;
                    info!("Hotkeys reloaded after config reload");
                }

                // Log if client disconnected before receiving response
                if responder.send(response).is_err() {
                    debug!("Client disconnected before receiving IPC response");
                }

                if is_toggle_pause {
                    let state = state.lock().await;
                    if let Some(ref mgr) = tray_manager {
                        mgr.update_pause_text(state.paused);
                        let wc = state.all_managed_window_ids().len();
                        let mc = state.monitors.len();
                        let aws = (state.active_workspace_idx(state.focused_monitor) + 1) as u8;
                        mgr.update_tooltip(
                            wc,
                            mc,
                            state.paused,
                            Some((hotkey_state.registered_count, hotkey_state.requested_count)),
                            aws,
                        );
                    }
                }

                // Show snap hint for resize operations
                if is_resize {
                    if let (Some(ref overlay), Some(rect)) = (&snap_hint_overlay, column_rect) {
                        // Cancel any pending hide timer
                        if let Some(handle) = snap_hint_timer_handle.take() {
                            handle.abort();
                        }

                        // Show the snap hint
                        overlay.show_snap_target(rect);

                        // Schedule hide after duration
                        let hide_tx = event_tx.clone();
                        let duration = hint_duration;
                        snap_hint_timer_handle = Some(tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(duration as u64))
                                .await;
                            let _ = hide_tx.send(DaemonEvent::HideSnapHint).await;
                        }));
                    }
                }

                // Start animation if needed
                if should_animate && !animation_active {
                    let mut state = state.lock().await;
                    state.tick_animations(0);
                    if let Ok(true) = state.send_animation_frame(&animation_worker) {
                        animation_active = true;
                        last_frame_instant = Some(std::time::Instant::now());
                    }
                }
            }
            DaemonEvent::WindowEvent(win_event) => {
                // Handle MouseEnterWindow specially for focus-follows-mouse debouncing
                if let WindowEvent::MouseEnterWindow(hwnd) = win_event {
                    let (enabled, delay_ms) = {
                        let state = state.lock().await;
                        (
                            state.config.behavior.focus_follows_mouse,
                            state.config.behavior.focus_follows_mouse_delay_ms,
                        )
                    };

                    if enabled {
                        // Cancel any pending focus timer
                        if let Some(handle) = focus_follows_mouse_timer.take() {
                            handle.abort();
                        }

                        // Schedule focus after delay (debouncing)
                        let focus_tx = event_tx.clone();
                        let delay = delay_ms;
                        focus_follows_mouse_timer = Some(tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(delay as u64))
                                .await;
                            let _ = focus_tx
                                .send(DaemonEvent::FocusFollowsMouse { window_id: hwnd })
                                .await;
                        }));
                    }
                } else {
                    {
                        let mut state = state.lock().await;
                        state.handle_window_event(win_event);

                        // Start animation worker if the event triggered a transition.
                        if state.is_animating() && !animation_active {
                            state.tick_animations(0);
                            if let Ok(true) = state.send_animation_frame(&animation_worker) {
                                animation_active = true;
                                last_frame_instant = Some(std::time::Instant::now());
                            }
                        }

                        // Process drag hint overlay requests from event handler.
                        // Drag ghost always shows regardless of snap_hints.enabled.
                        if let Some(hint) = state.pending_drag_hint.take() {
                            if let Some(ref overlay) = snap_hint_overlay {
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
                        if let Some(ref mgr) = tray_manager {
                            let wc = state.all_managed_window_ids().len();
                            let mc = state.monitors.len();
                            let aws = (state.active_workspace_idx(state.focused_monitor) + 1) as u8;
                            mgr.update_tooltip(
                                wc,
                                mc,
                                state.paused,
                                Some((hotkey_state.registered_count, hotkey_state.requested_count)),
                                aws,
                            );
                        }
                        // Spawn vsync-aligned animation thread for resize preview transitions.
                        if let Some(req) = state.pending_resize_animation.take() {
                            if let Some(ref overlay) = snap_hint_overlay {
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
                        if state.is_animating() && !animation_active {
                            state.tick_animations(0);
                            if let Ok(true) = state.send_animation_frame(&animation_worker) {
                                animation_active = true;
                                last_frame_instant = Some(std::time::Instant::now());
                            }
                        }
                    }
                }
            }
            DaemonEvent::Hotkey(hotkey_event) => {
                let mut requested_shutdown: Option<ShutdownMode> = None;
                let (should_animate, is_resize, column_rect, hint_duration) =
                    if let Some(cmd) = hotkey_state.mapping.get(&hotkey_event.id).cloned() {
                        debug!("Hotkey {} triggered, executing {:?}", hotkey_event.id, cmd);
                        if let Some(mode) = shutdown_mode_for_command(&cmd) {
                            requested_shutdown = Some(mode);
                            (false, false, None, 200)
                        } else {
                            let is_resize = matches!(cmd, IpcCommand::Resize { .. });
                            let mut state = state.lock().await;
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
                    run_shutdown_cleanup(&state, mode).await;
                    break;
                }

                // Show snap hint for resize operations
                if is_resize {
                    if let (Some(ref overlay), Some(rect)) = (&snap_hint_overlay, column_rect) {
                        // Cancel any pending hide timer
                        if let Some(handle) = snap_hint_timer_handle.take() {
                            handle.abort();
                        }

                        // Show the snap hint
                        overlay.show_snap_target(rect);

                        // Schedule hide after duration
                        let hide_tx = event_tx.clone();
                        let duration = hint_duration;
                        snap_hint_timer_handle = Some(tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(duration as u64))
                                .await;
                            let _ = hide_tx.send(DaemonEvent::HideSnapHint).await;
                        }));
                    }
                }

                // Start animation if needed
                if should_animate && !animation_active {
                    let mut state = state.lock().await;
                    state.tick_animations(0);
                    if let Ok(true) = state.send_animation_frame(&animation_worker) {
                        animation_active = true;
                        last_frame_instant = Some(std::time::Instant::now());
                    }
                }
            }
            DaemonEvent::Gesture(gesture_event) => {
                // Map gesture to command from config
                let gesture_config = {
                    let state = state.lock().await;
                    state.config.gestures.clone()
                };

                let cmd_str = match gesture_event {
                    GestureEvent::SwipeLeft => &gesture_config.swipe_left,
                    GestureEvent::SwipeRight => &gesture_config.swipe_right,
                    GestureEvent::SwipeUp => &gesture_config.swipe_up,
                    GestureEvent::SwipeDown => &gesture_config.swipe_down,
                };

                if let Some(cmd) = config::parse_command(cmd_str) {
                    debug!("Gesture {:?} triggered, executing {:?}", gesture_event, cmd);
                    if let Some(mode) = shutdown_mode_for_command(&cmd) {
                        warn!(
                            "Gesture {:?} requested {}; running shutdown cleanup",
                            gesture_event,
                            mode.label()
                        );
                        run_shutdown_cleanup(&state, mode).await;
                        break;
                    }
                    {
                        let mut state = state.lock().await;
                        let response = state.handle_command(cmd);
                        if let IpcResponse::Error { message } = response {
                            warn!("Gesture command failed: {}", message);
                        }
                        if state.is_animating() && !animation_active {
                            state.tick_animations(0);
                            if let Ok(true) = state.send_animation_frame(&animation_worker) {
                                animation_active = true;
                                last_frame_instant = Some(std::time::Instant::now());
                            }
                        }
                    }
                } else {
                    warn!("Unknown command for gesture: {}", cmd_str);
                }
            }
            DaemonEvent::Tray(tray_event) => {
                match tray_event {
                    tray::TrayEvent::Refresh => {
                        info!("Tray: Refresh requested");
                        let mut state = state.lock().await;
                        let response = state.handle_command(IpcCommand::Refresh);
                        if let IpcResponse::Error { message } = response {
                            warn!("Refresh failed: {}", message);
                        }
                    }
                    tray::TrayEvent::Reload => {
                        info!("Tray: Reload config requested");
                        let response = {
                            let mut state = state.lock().await;
                            state.handle_command(IpcCommand::Reload)
                        };

                        // If config was reloaded successfully, also reload hotkeys
                        if matches!(response, IpcResponse::Ok) {
                            reload_config_and_hotkeys(
                                &state, &mut hotkey_state, &event_tx,
                                &tray_manager, &snap_hint_overlay,
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
                        let _ = event_tx.send(DaemonEvent::Shutdown).await;
                    }
                    tray::TrayEvent::TogglePause => {
                        let mut state = state.lock().await;
                        if let Err(e) = state.toggle_pause("tray toggle") {
                            warn!("Tray toggle pause failed: {}", e);
                        }
                        if let Some(ref mgr) = tray_manager {
                            mgr.update_pause_text(state.paused);
                            let wc = state.all_managed_window_ids().len();
                            let mc = state.monitors.len();
                            let aws = (state.active_workspace_idx(state.focused_monitor) + 1) as u8;
                            mgr.update_tooltip(
                                wc,
                                mc,
                                state.paused,
                                Some((hotkey_state.registered_count, hotkey_state.requested_count)),
                                aws,
                            );
                        }
                    }
                    tray::TrayEvent::OpenConfig => {
                        info!("Tray: Settings requested");
                        let config_snapshot = {
                            let st = state.lock().await;
                            st.config.clone()
                        };
                        _settings_handle = settings::SettingsWindowHandle::open(
                            config_snapshot,
                            settings_sync_tx.clone(),
                            None,
                        );
                    }
                    tray::TrayEvent::OpenAbout => {
                        info!("Tray: About requested");
                        let config_snapshot = {
                            let st = state.lock().await;
                            st.config.clone()
                        };
                        _settings_handle = settings::SettingsWindowHandle::open(
                            config_snapshot,
                            settings_sync_tx.clone(),
                            Some("about"),
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
                            let mut state = state.lock().await;
                            // 1. Pause tiling
                            if !state.paused {
                                let _ = state.toggle_pause("release all windows");
                            }
                            // 2. Clear focus and hide border
                            state.hide_border();
                            state.previous_focused_hwnd = None;
                            // 3. Cascade all managed windows
                            let window_ids = state.all_managed_window_ids();
                            cascade_windows(&window_ids);
                        }
                        if let Some(ref mgr) = tray_manager {
                            mgr.update_pause_text(true);
                        }
                        // 3. Ask user if they want to restart tiling
                        let event_tx_clone = event_tx.clone();
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
                        let mut state = state.lock().await;
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
                        let mut state = state.lock().await;
                        state.config.behavior.focus_new_windows =
                            !state.config.behavior.focus_new_windows;
                        info!(
                            "Tray: Focus new windows toggled to {}",
                            state.config.behavior.focus_new_windows
                        );
                        let _ = state.config.save();
                    }
                    tray::TrayEvent::ToggleFocusFollowsMouse => {
                        let mut state = state.lock().await;
                        state.config.behavior.focus_follows_mouse =
                            !state.config.behavior.focus_follows_mouse;
                        info!(
                            "Tray: Focus follows mouse toggled to {}",
                            state.config.behavior.focus_follows_mouse
                        );
                        let _ = state.config.save();
                    }
                    tray::TrayEvent::SetCenteringCenter => {
                        let mut state = state.lock().await;
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
                        if let Some(ref mgr) = tray_manager {
                            mgr.update_quick_toggles(
                                state.config.appearance.active_border,
                                state.config.behavior.focus_new_windows,
                                state.config.behavior.focus_follows_mouse,
                                tray::CENTERING_CENTER,
                            );
                        }
                    }
                    tray::TrayEvent::SetCenteringJustInView => {
                        let mut state = state.lock().await;
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
                        if let Some(ref mgr) = tray_manager {
                            mgr.update_quick_toggles(
                                state.config.appearance.active_border,
                                state.config.behavior.focus_new_windows,
                                state.config.behavior.focus_follows_mouse,
                                tray::CENTERING_JUST_IN_VIEW,
                            );
                        }
                    }
                }
            }
            DaemonEvent::Settings(settings_event) => {
                match settings_event {
                    settings::SettingsEvent::Saved => {
                        info!("Settings: config saved, triggering reload");
                        let response = {
                            let mut state = state.lock().await;
                            state.handle_command(IpcCommand::Reload)
                        };
                        if matches!(response, IpcResponse::Ok) {
                            reload_config_and_hotkeys(
                                &state, &mut hotkey_state, &event_tx,
                                &tray_manager, &snap_hint_overlay,
                            ).await;
                            info!("Hotkeys reloaded after settings save");
                        } else if let IpcResponse::Error { message } = response {
                            warn!("Reload after settings save failed: {}", message);
                        }
                    }
                }
            }
            DaemonEvent::AnimationFrameApplied(frame_result) => {
                {
                    let mut state = state.lock().await;
                    state.applying_layout = false;
                    // Feed width violations back to the layout engine so it can
                    // allocate correct column widths on subsequent frames.
                    if !frame_result.width_violations.is_empty() {
                        for violation in &frame_result.width_violations {
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
                    // Reposition border to follow the focused window during animation.
                    if let Some(hwnd) = state.previous_focused_hwnd {
                        if state.config.appearance.active_border {
                            state.show_border(hwnd);
                        }
                    }
                }
                if let Err(ref e) = frame_result.apply_result {
                    warn!("Animation frame failed: {}", e);
                }
                // Measure real elapsed time (cap at 100ms to prevent jump from stalls)
                let delta_ms = last_frame_instant
                    .map(|t| t.elapsed().as_millis().min(100) as u64)
                    .unwrap_or(16);
                last_frame_instant = Some(std::time::Instant::now());

                let still_animating = {
                    let mut state = state.lock().await;
                    let running = state.tick_animations(delta_ms);
                    if running || state.is_animating() {
                        matches!(
                            state.send_animation_frame(&animation_worker),
                            Ok(true)
                        )
                    } else {
                        false
                    }
                };
                if !still_animating {
                    animation_active = false;
                    last_frame_instant = None;
                    // Final landing pass: apply exact resting positions.
                    // The last animation frame was at an interpolated offset
                    // slightly before the target; this pass uses the exact
                    // scroll_offset (set to target by tick_animation) and
                    // bypasses the worker cache to reposition every window.
                    {
                        let mut state = state.lock().await;
                        if let Err(e) = state.apply_layout() {
                            warn!("Final landing layout failed: {}", e);
                        }
                    }
                    debug!("All animations complete");
                }
            }
            DaemonEvent::HideSnapHint => {
                if let Some(ref overlay) = snap_hint_overlay {
                    overlay.hide();
                    debug!("Snap hint hidden");
                }
            }
            DaemonEvent::FocusFollowsMouse { window_id } => {
                let mut state = state.lock().await;
                if state.config.behavior.focus_follows_mouse {
                    let applied = state.apply_focus_follows_mouse(window_id);
                    if applied && state.is_animating() && !animation_active {
                        state.tick_animations(0);
                        if let Ok(true) = state.send_animation_frame(&animation_worker) {
                            animation_active = true;
                            last_frame_instant = Some(std::time::Instant::now());
                        }
                    }
                }
            }
            DaemonEvent::Shutdown => {
                info!("Shutdown signal received");
                run_shutdown_cleanup(&state, ShutdownMode::Graceful).await;
                break;
            }
        }
    }

    // Clean up animation worker (Drop sends Shutdown and joins)
    drop(animation_worker);

    // Clean up timers if running
    if let Some(handle) = snap_hint_timer_handle {
        handle.abort();
    }
    if let Some(handle) = focus_follows_mouse_timer {
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
