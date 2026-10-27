//! One active chrome read and one replaceable follow-up, never a thread per focus event.

use std::sync::{Arc, Mutex};

type Read = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct State {
    running: bool,
    pending: Option<Read>,
}

#[derive(Default)]
pub(super) struct ChromeRefresh(Arc<Mutex<State>>);

impl ChromeRefresh {
    pub(super) fn request(&self, read: impl FnOnce() + Send + 'static) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending = Some(Box::new(read));
        if state.running {
            return;
        }
        state.running = true;
        let shared = Arc::clone(&self.0);
        std::thread::spawn(move || {
            loop {
                let mut state = shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(read) = state.pending.take() else {
                    state.running = false;
                    return;
                };
                drop(state);
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(read)).is_err() {
                    tracing::warn!("composer chrome refresh panicked; worker remains available");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_panicked_read_does_not_strand_pending_or_future_refreshes() {
        let refresh = super::ChromeRefresh::default();
        let (entered, seen) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let first = entered.clone();
        refresh.request(move || {
            first.send(0).unwrap();
            blocked.recv().unwrap();
            panic!("injected chrome read panic");
        });
        assert_eq!(
            seen.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            0
        );
        let pending = entered.clone();
        refresh.request(move || {
            pending.send(1).unwrap();
        });
        release.send(()).unwrap();
        assert_eq!(
            seen.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            1
        );
        refresh.request(move || {
            entered.send(2).unwrap();
        });
        assert_eq!(
            seen.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            2
        );
    }

    #[test]
    fn focus_bursts_have_one_active_read_and_one_latest_follow_up() {
        let refresh = super::ChromeRefresh::default();
        let (entered, seen) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let first = entered.clone();
        refresh.request(move || {
            first.send((0, std::thread::current().id())).unwrap();
            blocked.recv().unwrap();
        });
        let (number, worker) = seen
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(number, 0);
        for n in 1..100 {
            let entered = entered.clone();
            refresh.request(move || {
                entered.send((n, std::thread::current().id())).unwrap();
            });
        }
        assert!(
            seen.try_recv().is_err(),
            "no second read may run alongside the blocked one"
        );
        release.send(()).unwrap();
        assert_eq!(
            seen.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            (99, worker)
        );
        assert!(seen.try_recv().is_err());
    }
}
