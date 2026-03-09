//! Persistent animation worker thread using DwmFlush for vsync-aligned frame pacing.
//!
//! Instead of spawning a new OS thread per animation frame (the old `AnimationTick` approach),
//! this module maintains a single worker thread that:
//! 1. Blocks on a channel when idle (zero CPU)
//! 2. Applies window placements via DeferWindowPos
//! 3. Calls DwmFlush() to block until the next compositor vsync
//! 4. Sends the result back to the main event loop
//!
//! This eliminates per-frame thread spawn overhead and naturally adapts to any refresh rate.

use leopardwm_core_layout::WindowPlacement;
use leopardwm_platform_win32::{PlacementCache, PlatformConfig};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};
use tracing::debug;

/// Data sent to the worker for each animation frame.
pub struct FrameRequest {
    pub placements: Vec<WindowPlacement>,
    pub platform_config: PlatformConfig,
}

/// Result sent back from the worker after applying a frame.
pub struct FrameResult {
    /// Whether the placements were applied successfully.
    pub apply_result: Result<(), String>,
    /// How long the frame took (apply + vsync wait).
    #[allow(dead_code)]
    pub frame_time: Duration,
    /// Width violations detected (windows enforcing a minimum width).
    pub width_violations: Vec<leopardwm_platform_win32::WidthViolation>,
}

/// Commands the main thread can send to the worker.
enum WorkerCommand {
    /// Apply a frame of animation.
    Frame(FrameRequest),
    /// Shut down the worker thread.
    Shutdown,
}

/// Handle to the persistent animation worker thread.
pub struct AnimationWorkerHandle {
    command_tx: std_mpsc::Sender<WorkerCommand>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AnimationWorkerHandle {
    /// Spawn the animation worker thread.
    ///
    /// The worker blocks on channel recv when idle, consuming no CPU.
    /// `event_tx` is used to send `FrameResult` back to the main event loop.
    pub fn spawn(
        event_tx: tokio::sync::mpsc::Sender<super::DaemonEvent>,
    ) -> Result<Self, std::io::Error> {
        let (command_tx, command_rx) = std_mpsc::channel::<WorkerCommand>();

        let thread = std::thread::Builder::new()
            .name("leopardwm-animation-worker".to_string())
            .spawn(move || {
                worker_loop(command_rx, event_tx);
            })?;

        Ok(Self {
            command_tx,
            thread: Some(thread),
        })
    }

    /// Send a frame request to the worker.
    ///
    /// Returns `Ok(())` if the request was queued, `Err` if the worker has exited.
    pub fn send_frame(&self, request: FrameRequest) -> Result<(), String> {
        self.command_tx
            .send(WorkerCommand::Frame(request))
            .map_err(|_| "Animation worker thread has exited".to_string())
    }
}

impl Drop for AnimationWorkerHandle {
    fn drop(&mut self) {
        // Send shutdown command (ignore error if worker already exited)
        let _ = self.command_tx.send(WorkerCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The worker thread's main loop.
///
/// Blocks on `command_rx.recv()` when no animation is active (zero CPU).
/// For each frame: apply placements → DwmFlush → send result back.
fn worker_loop(
    command_rx: std_mpsc::Receiver<WorkerCommand>,
    event_tx: tokio::sync::mpsc::Sender<super::DaemonEvent>,
) {
    debug!("Animation worker thread started");
    let mut placement_cache = PlacementCache::new();

    loop {
        // Block until we receive a command (zero CPU when idle)
        let command = match command_rx.recv() {
            Ok(cmd) => cmd,
            Err(_) => {
                debug!("Animation worker: command channel closed, exiting");
                break;
            }
        };

        match command {
            WorkerCommand::Shutdown => {
                debug!("Animation worker: shutdown requested");
                break;
            }
            WorkerCommand::Frame(request) => {
                let frame_start = Instant::now();

                // Apply window placements, skipping unchanged windows via cache
                let (apply_result, width_violations) =
                    match leopardwm_platform_win32::apply_placements(
                        &request.placements,
                        &request.platform_config,
                        Some(&mut placement_cache),
                    ) {
                        Ok(r) => (Ok(()), r.width_violations),
                        Err(e) => (Err(e.to_string()), Vec::new()),
                    };

                // Wait for next vsync via DwmFlush
                dwm_flush_or_fallback();

                let frame_time = frame_start.elapsed();

                let result = FrameResult {
                    apply_result,
                    frame_time,
                    width_violations,
                };

                // Send result back to main event loop
                if event_tx
                    .blocking_send(super::DaemonEvent::AnimationFrameApplied(result))
                    .is_err()
                {
                    debug!("Animation worker: event channel closed, exiting");
                    break;
                }
            }
        }
    }

    debug!("Animation worker thread exiting");
}

/// Call DwmFlush to wait for the next compositor vsync.
/// Falls back to a 1ms sleep if DWM is unavailable (e.g. Remote Desktop, basic theme).
fn dwm_flush_or_fallback() {
    use windows::Win32::Graphics::Dwm::DwmFlush;

    let result = unsafe { DwmFlush() };
    if result.is_err() {
        // DWM not available — sleep briefly so we don't spin
        std::thread::sleep(Duration::from_millis(1));
    }
}
