use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

pub(crate) struct KeepAwake {
    sender: Option<Sender<bool>>,
    thread: Option<JoinHandle<()>>,
}

impl KeepAwake {
    pub(crate) fn new(apply: impl Fn(bool) + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("keep-awake".into())
            .spawn(move || {
                let mut current = false;
                while let Ok(on) = receiver.recv() {
                    if on != current {
                        apply(on);
                        current = on;
                    }
                }
                if current {
                    apply(false);
                }
            })
            .expect("failed to spawn keep-awake thread");
        Self {
            sender: Some(sender),
            thread: Some(thread),
        }
    }

    pub(crate) fn set(&self, on: bool) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(on);
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn changes_and_drop_use_one_thread() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let awake = KeepAwake::new(move |on| {
            recorded.lock().unwrap().push((on, thread::current().id()));
        });
        awake.set(true);
        awake.set(true);
        awake.set(false);
        awake.set(true);
        drop(awake);
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.iter().map(|(on, _)| *on).collect::<Vec<_>>(),
            [true, false, true, false]
        );
        assert!(calls.iter().all(|(_, id)| *id == calls[0].1));
        assert_ne!(calls[0].1, thread::current().id());
    }
}
