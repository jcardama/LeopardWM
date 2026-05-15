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

use leopardwm_core_layout::{Rect, WindowPlacement};
use leopardwm_platform_win32::{PlacementCache, PlatformConfig};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};
use tracing::debug;

/// Data sent to the worker for each animation frame.
pub struct FrameRequest {
    /// Placements driven via per-frame `SetWindowPos` on the live HWND.
    /// Excludes any windows being driven via DWM thumbnail (those are in
    /// `ghost_updates`).
    pub placements: Vec<WindowPlacement>,
    /// Thumbnail destination-rect updates for windows being ghost-animated
    /// this frame. The worker calls `DwmUpdateThumbnailProperties` for
    /// each before `DwmFlush`, so live and ghost windows arrive on the
    /// same vsync.
    pub ghost_updates: Vec<GhostFrame>,
    pub platform_config: PlatformConfig,
}

/// Per-frame thumbnail update payload. `handle_isize` is a raw
/// `HTHUMBNAIL` value (sender owns the registration; worker only updates).
pub struct GhostFrame {
    pub handle_isize: isize,
    /// Destination rect in client coordinates of the thumbnail host.
    pub dest_client_rect: Rect,
    pub opacity: u8,
    pub visible: bool,
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
    /// Height violations detected (windows enforcing a minimum height).
    pub height_violations: Vec<leopardwm_platform_win32::HeightViolation>,
}

/// Commands the main thread can send to the worker.
enum WorkerCommand {
    /// Apply a frame of animation.
    Frame(FrameRequest),
    /// Run an 8-frame crossfade on owned thumbnail handles, then drop them.
    /// Worker takes ownership of `entries`; each `CrossfadeEntry::Drop`
    /// calls `thumbnail::unregister_raw`, so panic-unwind and normal exit
    /// both unregister cleanly. After the fade (or abort), worker sends
    /// `DaemonEvent::CrossfadeComplete { epoch }`.
    Crossfade {
        epoch: u64,
        entries: Vec<CrossfadeEntry>,
        frames: u32,
    },
    /// Tell the worker to break out of an in-flight fade for this epoch.
    /// Mismatching-epoch aborts are discarded. Cooperative: worker only
    /// checks between fade iterations via `try_recv`.
    AbortCrossfade { epoch: u64 },
    /// Invalidate the placement cache (e.g., after a theme/display change
    /// so stale inset-expanded positions don't survive as cache hits).
    ClearCache,
    /// Shut down the worker thread.
    Shutdown,
}

/// Worker-owned thumbnail handle during a crossfade. Drop unregisters,
/// so panic-unwind and normal end-of-fade both unregister cleanly.
pub struct CrossfadeEntry {
    pub handle_isize: isize,
    pub dest_client_rect: Rect,
}

impl Drop for CrossfadeEntry {
    fn drop(&mut self) {
        if self.handle_isize != 0 {
            leopardwm_platform_win32::thumbnail::unregister_raw(self.handle_isize);
            self.handle_isize = 0;
        }
    }
}

/// Handle to the persistent animation worker thread.
pub struct AnimationWorkerHandle {
    command_tx: std_mpsc::Sender<WorkerCommand>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Cloneable remote control for an `AnimationWorkerHandle`. Distributed
/// across the daemon so any code path can send `AbortCrossfade` without
/// holding the owning handle.
///
/// Only exposes commands safe for arbitrary callers; full lifecycle (e.g.
/// `Shutdown`) stays gated to the owner.
#[derive(Clone)]
pub struct AnimationWorkerControl {
    command_tx: std_mpsc::Sender<WorkerCommand>,
}

impl AnimationWorkerControl {
    /// Signal the worker to abort an in-flight crossfade for `epoch`.
    /// Cooperative — the worker only checks between fade iterations.
    pub fn send_abort_crossfade(&self, epoch: u64) {
        let _ = self.command_tx.send(WorkerCommand::AbortCrossfade { epoch });
    }
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

    /// Return a cloneable remote-control handle usable from AppState
    /// helpers that don't have direct access to this owner.
    pub fn control(&self) -> AnimationWorkerControl {
        AnimationWorkerControl {
            command_tx: self.command_tx.clone(),
        }
    }

    /// Invalidate the worker's placement cache. Call after theme/display changes
    /// so that stale inset-expanded positions don't survive as cache hits.
    pub fn clear_cache(&self) {
        let _ = self.command_tx.send(WorkerCommand::ClearCache);
    }

    /// Send a crossfade command to the worker. The worker takes ownership
    /// of `entries` for the duration of the fade and unregisters each
    /// thumbnail on completion (via `CrossfadeEntry::Drop`).
    pub fn send_crossfade(
        &self,
        epoch: u64,
        entries: Vec<CrossfadeEntry>,
        frames: u32,
    ) -> Result<(), String> {
        self.command_tx
            .send(WorkerCommand::Crossfade {
                epoch,
                entries,
                frames,
            })
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
    // Single-slot buffer for commands preempting an in-flight crossfade.
    // Always consumed before next channel `recv()`.
    let mut pending: Option<WorkerCommand> = None;

    loop {
        // Block until we receive a command (zero CPU when idle), unless a
        // pending command was buffered by a preempted crossfade — then we
        // process it before touching the channel.
        let command = match pending.take() {
            Some(cmd) => cmd,
            None => match command_rx.recv() {
                Ok(cmd) => cmd,
                Err(_) => {
                    debug!("Animation worker: command channel closed, exiting");
                    break;
                }
            },
        };

        match command {
            WorkerCommand::Shutdown => {
                debug!("Animation worker: shutdown requested");
                break;
            }
            WorkerCommand::ClearCache => {
                placement_cache.clear();
                placement_cache.clear_insets();
                debug!("Animation worker: placement cache cleared");
                continue;
            }
            WorkerCommand::Crossfade {
                epoch,
                entries,
                frames,
            } => {
                run_crossfade(&command_rx, &event_tx, epoch, entries, frames, &mut pending);
                continue;
            }
            WorkerCommand::AbortCrossfade { epoch: _ } => {
                // No fade in flight at the outer-loop level. Discard.
                continue;
            }
            WorkerCommand::Frame(request) => {
                let frame_start = Instant::now();

                // Apply window placements, skipping unchanged windows via cache.
                // Animation frames are SWP_ASYNCWINDOWPOS so the sticky-compositor
                // nudge inside `apply_placements` is a no-op; pass `false` to keep
                // the call signature explicit.
                let (apply_result, width_violations, height_violations) =
                    match leopardwm_platform_win32::apply_placements(
                        &request.placements,
                        &request.platform_config,
                        Some(&mut placement_cache),
                        false,
                    ) {
                        Ok(r) => (Ok(()), r.width_violations, r.height_violations),
                        Err(e) => (Err(e.to_string()), Vec::new(), Vec::new()),
                    };

                // Apply per-frame thumbnail updates for ghost-animated windows.
                // Failures are logged but don't fail the frame — a ghost that
                // misses a single frame is better than a stalled animation.
                for g in &request.ghost_updates {
                    if let Err(e) = leopardwm_platform_win32::thumbnail::update(
                        g.handle_isize,
                        g.dest_client_rect,
                        g.opacity,
                        g.visible,
                    ) {
                        debug!("thumbnail::update failed: {}", e);
                    }
                }

                // Wait for next vsync via DwmFlush
                dwm_flush_or_fallback();

                let frame_time = frame_start.elapsed();

                let result = FrameResult {
                    apply_result,
                    frame_time,
                    width_violations,
                    height_violations,
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

/// Run a cooperative crossfade on owned thumbnail entries. Between each
/// of the 8 (typical) ease-in-cubic fade iterations, try_recv the
/// command channel:
/// - Matching `AbortCrossfade { epoch }` → break early.
/// - Mismatching abort → discard.
/// - Any other command → buffer in `pending` for the outer loop to
///   process next, break early.
///
/// In all exit paths, `entries` drops here — each `CrossfadeEntry::Drop`
/// calls `thumbnail::unregister_raw`, so panic-unwind and normal exit
/// both unregister cleanly. After cleanup, emits `CrossfadeComplete { epoch }`
/// so the daemon can clear `active_crossfade` and release the
/// same-source-re-registration barrier.
fn run_crossfade(
    command_rx: &std_mpsc::Receiver<WorkerCommand>,
    event_tx: &tokio::sync::mpsc::Sender<super::DaemonEvent>,
    epoch: u64,
    entries: Vec<CrossfadeEntry>,
    frames: u32,
    pending: &mut Option<WorkerCommand>,
) {
    use std::sync::mpsc::TryRecvError;

    let mut aborted = false;
    for i in 0..frames {
        // Cooperative preempt/abort check.
        match command_rx.try_recv() {
            Ok(WorkerCommand::AbortCrossfade { epoch: e }) if e == epoch => {
                aborted = true;
                break;
            }
            Ok(WorkerCommand::AbortCrossfade { .. }) => {
                // Mismatched epoch — discard (stale).
            }
            Ok(other) => {
                // Preempt by another Frame / Crossfade / ClearCache /
                // Shutdown. Buffer it and exit so outer loop processes
                // it next, after entries drop and CrossfadeComplete
                // is sent.
                *pending = Some(other);
                break;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                aborted = true;
                break;
            }
        }

        // Ease-in-cubic: opacity starts at 255 and decays.
        // t in (0, 1]; opacity = (1 - t³) * 255.
        let t = (i + 1) as f64 / frames as f64;
        let opacity = ((1.0 - t.powi(3)) * 255.0).round().clamp(0.0, 255.0) as u8;
        for entry in &entries {
            if let Err(e) = leopardwm_platform_win32::thumbnail::update(
                entry.handle_isize,
                entry.dest_client_rect,
                opacity,
                opacity > 0,
            ) {
                debug!("crossfade thumbnail::update failed: {}", e);
            }
        }
        dwm_flush_or_fallback();
    }

    // Drop entries here — each CrossfadeEntry::Drop calls unregister_raw,
    // regardless of normal completion or early abort.
    drop(entries);

    if aborted {
        debug!("Animation worker: crossfade epoch {} aborted", epoch);
    }
    let _ = event_tx.blocking_send(super::DaemonEvent::CrossfadeComplete { epoch });
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
