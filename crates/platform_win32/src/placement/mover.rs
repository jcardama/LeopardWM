use super::{cached_placement_owner_answer, placement_owner_answers, DeferEntry};
use crate::window_id_to_hwnd;
use leopardwm_core_layout::WindowId;
use leopardwm_ipc::log_text;
use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, GetWindowThreadProcessId, SetWindowPos, SET_WINDOW_POS_FLAGS, SWP_ASYNCWINDOWPOS,
};

#[derive(Clone, Copy, PartialEq, Eq)]
struct Target {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    flags: SET_WINDOW_POS_FLAGS,
    frame_refresh: Option<u64>,
    generation: u64,
}

#[derive(Clone, Copy)]
struct Correction {
    target: Option<Target>,
    generation: u64,
    before: Option<RECT>,
    deadline: Option<Instant>,
}

struct Slot {
    lifetime: u64,
    pending: Option<Target>,
    correction: Option<Correction>,
    completed: Option<(Target, RECT)>,
    generation: u64,
    cancelled: u64,
}

#[derive(Default)]
struct State {
    slots: HashMap<WindowId, Slot>,
    ready: VecDeque<WindowId>,
    next_generation: u64,
    in_flight: Option<(WindowId, u64)>,
    started: bool,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));
static WAKE: Condvar = Condvar::new();

fn lock() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(crate::recover_poisoned_mutex)
}

pub(super) fn generations() -> HashMap<WindowId, u64> {
    lock()
        .slots
        .iter()
        .map(|(&id, slot)| (id, slot.generation))
        .collect()
}

pub(super) fn mark(window_id: WindowId) {
    let mut state = lock();
    if state.slots.contains_key(&window_id) {
        return;
    }
    // Pre-mark posted moves can land late; the next active tiling apply corrects them.
    // While paused (including release-all) or exited, correction waits until tiling resumes.
    state.next_generation += 1;
    let lifetime = state.next_generation;
    state.slots.insert(
        window_id,
        Slot {
            lifetime,
            pending: None,
            correction: None,
            completed: None,
            generation: lifetime,
            cancelled: 0,
        },
    );
    drop(state);

    let (class, executable) = window_id_to_hwnd(window_id)
        .map(|hwnd| {
            let class = super::window_class_name(hwnd);
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            (class, crate::get_process_executable(pid))
        })
        .unwrap_or_default();
    tracing::info!(
        "Window ignores queued moves; LeopardWM now moves it synchronously on the mover thread: hwnd={:#x} class={} executable={}",
        window_id,
        log_text::name(&class),
        log_text::name(executable.as_deref().unwrap_or(""))
    );
}

pub(super) fn is_marked(window_id: WindowId) -> bool {
    lock().slots.contains_key(&window_id)
}

pub(super) fn pending(window_id: WindowId) -> bool {
    let state = lock();
    match state.slots.get(&window_id) {
        Some(slot) => {
            state.in_flight == Some((window_id, slot.lifetime))
                || slot.correction.is_some()
                || slot.pending.is_some()
        }
        None => state.in_flight.is_some_and(|(id, _)| id == window_id),
    }
}

pub(super) fn seal(entry: &DeferEntry) -> bool {
    let window_id = entry.window_id;
    let mut rect = RECT::default();
    let observed = unsafe { GetWindowRect(entry.hwnd, &mut rect) }
        .ok()
        .map(|()| rect);
    let mut state = lock();
    let Some(slot) = state.slots.get(&window_id) else {
        return false;
    };
    let lifetime = slot.lifetime;
    let target = Target {
        x: entry.x,
        y: entry.y,
        w: entry.w,
        h: entry.h,
        flags: entry.flags & !SWP_ASYNCWINDOWPOS,
        frame_refresh: entry.frame_refresh,
        generation: slot.generation,
    };
    if state.in_flight != Some((window_id, lifetime))
        && slot.pending.is_none()
        && slot.correction.is_none()
        && slot
            .completed
            .is_some_and(|(completed, rect)| completed == target && observed == Some(rect))
    {
        return false;
    }
    state.next_generation += 1;
    let generation = state.next_generation;
    let in_flight = state.in_flight == Some((window_id, lifetime));
    if let Some(slot) = state.slots.get_mut(&window_id) {
        slot.generation = generation;
        slot.pending = None;
        slot.completed = None;
        slot.correction = Some(Correction {
            target: Some(Target {
                generation,
                ..target
            }),
            generation,
            before: None,
            deadline: None,
        });
    }
    state.ready.retain(|&id| id != window_id);
    if !in_flight {
        state.ready.push_back(window_id);
    }
    start(&mut state);
    true
}

pub(super) fn forget(window_id: WindowId) {
    let mut state = lock();
    state.slots.remove(&window_id);
    state.ready.retain(|&id| id != window_id);
}

pub(super) fn cancel(window_ids: Option<&[WindowId]>) {
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut state = lock();
    let affected = |id: WindowId| window_ids.is_none_or(|ids| ids.contains(&id));
    state.ready.retain(|&id| !affected(id));
    state.next_generation += 1;
    let generation = state.next_generation;
    for (&id, slot) in &mut state.slots {
        if affected(id) {
            slot.generation = generation;
            slot.cancelled = slot.generation;
            slot.pending = None;
            slot.correction = None;
            slot.completed = None;
        }
    }
    while state.in_flight.is_some_and(|(id, _)| affected(id)) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        state = WAKE
            .wait_timeout(state, remaining)
            .unwrap_or_else(|error| error.into_inner())
            .0;
    }
}

pub(super) fn submit(entry: &DeferEntry, generation: Option<u64>) {
    let mut state = lock();
    let Some(slot) = state.slots.get_mut(&entry.window_id) else {
        return;
    };
    if Some(slot.generation) != generation {
        return;
    }
    let generation = slot.generation;
    slot.completed = None;
    slot.pending = Some(Target {
        x: entry.x,
        y: entry.y,
        w: entry.w,
        h: entry.h,
        flags: entry.flags & !SWP_ASYNCWINDOWPOS,
        frame_refresh: entry.frame_refresh,
        generation,
    });
    if !state.ready.contains(&entry.window_id) {
        state.ready.push_back(entry.window_id);
    }
    start(&mut state);
}

fn start(state: &mut State) {
    if !state.started {
        match std::thread::Builder::new()
            .name("placement-mover".into())
            .spawn(run)
        {
            Ok(_) => state.started = true,
            Err(error) => tracing::warn!(%error, "Could not start placement mover"),
        }
    }
    WAKE.notify_one();
}

fn run() {
    loop {
        let (window_id, lifetime, target, correction) = {
            let mut state = lock();
            loop {
                if let Some(id) = state.ready.pop_front() {
                    let slot = state.slots.get_mut(&id).unwrap();
                    let correction = slot.correction.take().map(|mut correction| {
                        correction
                            .deadline
                            .get_or_insert_with(|| Instant::now() + Duration::from_secs(2));
                        correction
                    });
                    let target = if correction.is_some() {
                        None
                    } else {
                        slot.pending.take()
                    };
                    let lifetime = slot.lifetime;
                    state.in_flight = Some((id, lifetime));
                    break (id, lifetime, target, correction);
                }
                state = WAKE
                    .wait(state)
                    .unwrap_or_else(crate::recover_poisoned_mutex);
            }
        };
        let generation =
            correction.map_or_else(|| target.unwrap().generation, |work| work.generation);
        let mut before = None;
        let mut moved = false;
        let mut retry = None;
        let mut completed = None;
        if let Some(work) = correction.filter(|work| {
            work.deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        }) {
            tracing::warn!(
                window_id,
                generation = work.generation,
                "Dropping expired placement correction"
            );
        } else if let Ok(hwnd) = window_id_to_hwnd(window_id) {
            let answered = if correction.is_some() {
                placement_owner_answers(hwnd, 50)
            } else {
                cached_placement_owner_answer(hwnd)
            };
            if answered {
                let mut rect = RECT::default();
                before = unsafe { GetWindowRect(hwnd, &mut rect) }
                    .ok()
                    .map(|()| rect);
            }
            let is_current = || {
                lock()
                    .slots
                    .get(&window_id)
                    .is_some_and(|slot| slot.lifetime == lifetime && slot.generation == generation)
            };
            if is_current() && answered {
                moved = true;
                // A probe-to-hang race can stall this one mover, never the animation worker.
                if let Some(target) = correction.and_then(|work| work.target).or(target) {
                    if let Err(error) = unsafe {
                        SetWindowPos(
                            hwnd,
                            None,
                            target.x,
                            target.y,
                            target.w,
                            target.h,
                            target.flags,
                        )
                    } {
                        tracing::warn!(window_id, %error, "Placement mover could not apply target");
                    } else {
                        if let Some(mark) = target.frame_refresh {
                            super::clear_frame_refresh_at_target(
                                window_id,
                                hwnd,
                                mark,
                                leopardwm_core_layout::Rect::new(
                                    target.x, target.y, target.w, target.h,
                                ),
                                target.flags,
                            );
                        }
                        if correction.is_some() {
                            let mut rect = RECT::default();
                            completed = unsafe { GetWindowRect(hwnd, &mut rect) }
                                .ok()
                                .map(|()| (target, rect));
                        }
                    }
                } else {
                    crate::visibility::recover_after_placement_mover(
                        window_id,
                        correction.unwrap().before,
                        is_current,
                    );
                }
            } else if is_current() {
                retry = correction;
            }
        }
        finish(
            window_id, lifetime, generation, before, moved, retry, completed,
        );
    }
}

fn finish(
    window_id: WindowId,
    lifetime: u64,
    generation: u64,
    before: Option<RECT>,
    moved: bool,
    retry: Option<Correction>,
    completed: Option<(Target, RECT)>,
) {
    let mut state = lock();
    let has_work = state
        .slots
        .get_mut(&window_id)
        .filter(|slot| slot.lifetime == lifetime)
        .map(|slot| {
            if slot.generation == generation {
                slot.correction = retry;
                slot.completed = completed;
            } else if slot.correction.is_none()
                && moved
                && slot.cancelled > generation
                && slot.generation == slot.cancelled
            {
                slot.correction = Some(Correction {
                    target: None,
                    generation: slot.generation,
                    before,
                    deadline: None,
                });
            }
            slot.correction.is_some() || slot.pending.is_some()
        });
    if let Some(has_work) = has_work {
        state.ready.retain(|&id| id != window_id);
        if has_work {
            state.ready.push_back(window_id);
        }
    }
    state.in_flight = None;
    WAKE.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placement::InsetSource;
    use leopardwm_core_layout::Visibility;
    use windows::Win32::Foundation::HWND;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct LogWriter(std::sync::Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn marking_logs_once_per_window_lifetime() {
        let _guard = TEST_LOCK.lock().unwrap();
        let window_id = u64::MAX - 1;
        let output = std::sync::Arc::new(Mutex::new(Vec::new()));
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .without_time()
            .with_ansi(false)
            .with_writer(move || LogWriter(writer.clone()))
            .finish();
        let lines = || {
            String::from_utf8(output.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };

        let (first, repeated, reused) = tracing::subscriber::with_default(subscriber, || {
            mark(window_id);
            let first = lines();
            mark(window_id);
            let repeated = lines();
            forget(window_id);
            mark(window_id);
            let reused = lines();
            forget(window_id);
            (first, repeated, reused)
        });

        assert_eq!(first.len(), 1, "newly marked window must be logged");
        assert_eq!(repeated, first, "repeated marking must not log again");
        assert_eq!(reused.len(), 2, "a new window lifetime must log again");
        for line in reused {
            assert!(line.contains("INFO"));
            assert!(line.contains(&format!("hwnd={window_id:#x}")));
            assert!(line.contains("class="));
            assert!(line.contains("executable="));
        }
    }

    #[test]
    fn reused_window_landing_does_not_inherit_old_in_flight_work() {
        let _guard = TEST_LOCK.lock().unwrap();
        let window_id = u64::MAX;
        mark(window_id);
        let lifetime = lock().slots[&window_id].lifetime;
        lock().in_flight = Some((window_id, lifetime));
        let old_pending = pending(window_id);
        forget(window_id);
        mark(window_id);
        let pending_before_landing = pending(window_id);
        let entry = DeferEntry {
            hwnd: HWND::default(),
            window_id,
            x: 100,
            y: 100,
            w: 320,
            h: 240,
            layout_w: 320,
            layout_h: 240,
            insets: (0, 0, 0, 0),
            inset_source: InsetSource::Fresh,
            inset_generation: 0,
            visibility: Visibility::Visible,
            flags: SET_WINDOW_POS_FLAGS::default(),
            column_index: 0,
            offscreen_rect: None,
            frame_refresh: None,
        };
        let previously_started = lock().started;
        lock().started = true;
        let deferred = seal(&entry);
        finish(window_id, lifetime, lifetime, None, true, None, None);
        let (queued, new_lifetime, generation) = {
            let mut state = lock();
            let queued = state.ready.pop_front() == Some(window_id);
            let slot = state.slots.get_mut(&window_id).unwrap();
            slot.correction.take();
            let lifetime = slot.lifetime;
            let generation = slot.generation;
            state.in_flight = Some((window_id, lifetime));
            (queued, lifetime, generation)
        };
        finish(window_id, new_lifetime, generation, None, true, None, None);
        let pending_after_return = pending(window_id);
        forget(window_id);
        lock().started = previously_started;
        assert!(old_pending, "same-lifetime native work must remain pending");
        assert!(deferred, "marked landing must run on the mover");
        assert!(
            queued,
            "old-lifetime work stranded the new landing off the FIFO"
        );
        assert!(
            !pending_before_landing,
            "old-lifetime work blocked the new window's pending query"
        );
        assert!(
            !pending_after_return,
            "old-lifetime completion stranded the new landing"
        );
    }
}
