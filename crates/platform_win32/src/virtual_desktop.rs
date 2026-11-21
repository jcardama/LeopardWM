//! Bounded virtual-desktop queries on a dedicated COM apartment.

use leopardwm_core_layout::WindowId;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::Duration;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager};

const QUERY_TIMEOUT: Duration = Duration::from_millis(50);

type Request = (WindowId, mpsc::Sender<bool>);

trait Query {
    fn query(&mut self, hwnd: WindowId) -> Result<bool, ()>;
    fn after_reply(&mut self) {}
}

impl<F: FnMut(WindowId) -> Result<bool, ()>> Query for F {
    fn query(&mut self, hwnd: WindowId) -> Result<bool, ()> {
        self(hwnd)
    }
}

struct QueryWorker {
    sender: mpsc::Sender<Request>,
    in_flight: Arc<AtomicBool>,
}

impl QueryWorker {
    fn spawn<F, Q>(factory: F) -> Option<Self>
    where
        F: FnOnce() -> Q + Send + 'static,
        Q: Query + 'static,
    {
        let (sender, receiver) = mpsc::channel::<Request>();
        let in_flight = Arc::new(AtomicBool::new(false));
        let worker_in_flight = in_flight.clone();
        std::thread::Builder::new()
            .name("virtual-desktop-query".into())
            .spawn(move || {
                let mut query = factory();
                while let Ok((hwnd, reply)) = receiver.recv() {
                    let result = query.query(hwnd).unwrap_or(false);
                    worker_in_flight.store(false, Ordering::Release);
                    let _ = reply.send(result);
                    query.after_reply();
                }
            })
            .ok()?;
        Some(Self { sender, in_flight })
    }

    fn query(&self, hwnd: WindowId, timeout: Duration) -> bool {
        if self.in_flight.swap(true, Ordering::AcqRel) {
            return false;
        }
        let (reply, result) = mpsc::channel();
        if self.sender.send((hwnd, reply)).is_err() {
            self.in_flight.store(false, Ordering::Release);
            return false;
        }
        result.recv_timeout(timeout).unwrap_or(false)
    }
}

struct DesktopQuery {
    initialized: bool,
    manager: Option<IVirtualDesktopManager>,
}

impl DesktopQuery {
    fn new() -> Self {
        let mut query = Self {
            initialized: unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok() },
            manager: None,
        };
        query.recreate_manager();
        query
    }

    fn recreate_manager(&mut self) {
        if self.initialized && self.manager.is_none() {
            self.manager =
                unsafe { CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_ALL).ok() };
        }
    }
}

impl Query for DesktopQuery {
    fn query(&mut self, hwnd: WindowId) -> Result<bool, ()> {
        if !self.initialized {
            return Err(());
        }
        unsafe {
            let result = self
                .manager
                .as_ref()
                .ok_or(())?
                .IsWindowOnCurrentVirtualDesktop(HWND(hwnd as *mut c_void));
            match result {
                Ok(current) => Ok(current.as_bool()),
                Err(_) => {
                    self.manager = None;
                    Err(())
                }
            }
        }
    }

    fn after_reply(&mut self) {
        self.recreate_manager();
    }
}

impl Drop for DesktopQuery {
    fn drop(&mut self) {
        self.manager.take();
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

static WORKER: OnceLock<Option<QueryWorker>> = OnceLock::new();

fn start_worker<F, Q>(slot: &OnceLock<Option<QueryWorker>>, factory: F) -> Option<&QueryWorker>
where
    F: FnOnce() -> Q + Send + 'static,
    Q: Query + 'static,
{
    slot.get_or_init(|| QueryWorker::spawn(factory)).as_ref()
}

/// Start the desktop-query worker without waiting for COM initialization.
pub fn warm_virtual_desktop_queries() {
    let _ = start_worker(&WORKER, DesktopQuery::new);
}

/// Return whether the window is on the current Windows virtual desktop, failing closed.
pub fn is_window_on_current_virtual_desktop(hwnd: WindowId) -> bool {
    if hwnd == 0 {
        return false;
    }
    start_worker(&WORKER, DesktopQuery::new).is_some_and(|worker| worker.query(hwnd, QUERY_TIMEOUT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait_until_idle(worker: &QueryWorker) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while worker.in_flight.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "worker did not finish");
            std::thread::yield_now();
        }
    }

    #[test]
    fn warm_up_initializes_before_the_first_query() {
        let slot = Arc::new(OnceLock::new());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (returned_tx, returned_rx) = mpsc::channel();
        let thread_slot = slot.clone();
        let warm = std::thread::spawn(move || {
            start_worker(&thread_slot, move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                ready_tx.send(()).unwrap();
                |_| Ok(true)
            })
            .unwrap();
            returned_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let returned = returned_rx.recv_timeout(Duration::from_secs(1));
        let early_ready = ready_rx.recv_timeout(QUERY_TIMEOUT + Duration::from_millis(10));
        release_tx.send(()).unwrap();
        assert!(returned.is_ok(), "warm-up blocked on initialization");
        assert!(early_ready.is_err(), "factory was not delayed");
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        warm.join().unwrap();
        let worker = slot.get().unwrap().as_ref().unwrap();
        assert!(worker.query(1, QUERY_TIMEOUT));
    }

    #[test]
    fn recovery_runs_after_the_error_reply() {
        struct RecoveringQuery {
            recovered: bool,
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            finished: mpsc::Sender<()>,
        }
        impl Query for RecoveringQuery {
            fn query(&mut self, _: WindowId) -> Result<bool, ()> {
                if self.recovered {
                    Ok(true)
                } else {
                    Err(())
                }
            }
            fn after_reply(&mut self) {
                if !self.recovered {
                    self.entered.send(()).unwrap();
                    self.release.recv().unwrap();
                    self.recovered = true;
                    self.finished.send(()).unwrap();
                }
            }
        }
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let worker = QueryWorker::spawn(move || RecoveringQuery {
            recovered: false,
            entered: entered_tx,
            release: release_rx,
            finished: finished_tx,
        })
        .unwrap();
        assert!(!worker.query(1, QUERY_TIMEOUT));
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        release_tx.send(()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(worker.query(2, QUERY_TIMEOUT));
    }

    #[test]
    fn successive_completed_queries_get_their_answers() {
        let worker = QueryWorker::spawn(|| |_| Ok(true)).unwrap();
        for _ in 0..2000 {
            assert!(worker.query(1, Duration::from_secs(1)));
            assert!(worker.query(2, Duration::from_secs(1)));
        }
    }

    #[test]
    fn hung_query_times_out_and_rejects_concurrent_requests() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = QueryWorker::spawn(move || {
            move |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(true)
            }
        })
        .unwrap();
        let timeout = Duration::from_millis(30);
        let started = Instant::now();
        assert!(!worker.query(1, timeout));
        assert!(started.elapsed() < Duration::from_millis(250));
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        assert!(!worker.query(2, Duration::from_secs(1)));
        assert!(started.elapsed() < Duration::from_millis(250));
        release_tx.send(()).unwrap();
    }

    #[test]
    fn late_reply_cannot_answer_the_next_request() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        let worker = QueryWorker::spawn(move || {
            move |hwnd| {
                if hwnd == 1 {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(true)
                } else {
                    second_tx.send(hwnd).unwrap();
                    Ok(false)
                }
            }
        })
        .unwrap();
        assert!(!worker.query(1, Duration::from_millis(30)));
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        release_tx.send(()).unwrap();
        wait_until_idle(&worker);
        assert!(!worker.query(2, Duration::from_secs(1)));
        assert_eq!(second_rx.recv_timeout(Duration::from_secs(1)).unwrap(), 2);
    }

    #[test]
    fn query_error_fails_closed() {
        let (called_tx, called_rx) = mpsc::channel();
        let worker = QueryWorker::spawn(move || {
            move |_| {
                called_tx.send(()).unwrap();
                Err(())
            }
        })
        .unwrap();
        assert!(!worker.query(1, Duration::from_secs(1)));
        called_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }
}
