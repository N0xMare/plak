//! Latest-only background reads for terminal usage decoration.

use crate::home::paths::PlakPaths;
use crate::product_subscription_usage::annotate_limits;
use plakous_host_protocol::views::presentation::UsageLimitsView;

/// One latest-only display reader; neither mailbox lock is held across I/O.
pub(crate) struct UsageDecorationReader {
    shared: std::sync::Arc<(std::sync::Mutex<DecorationState>, std::sync::Condvar)>,
}

#[derive(Default)]
struct DecorationState {
    generation: u64,
    #[cfg(test)]
    reading: bool,
    pending: Option<(u64, UsageLimitsView)>,
    ready: Option<(u64, UsageLimitsView, Vec<String>)>,
    closed: bool,
}

impl UsageDecorationReader {
    pub(crate) fn new(paths: PlakPaths, zone: &'static jiff::tz::TimeZone, wake: fn()) -> Self {
        Self::with_reader(move |view| annotate_limits(view, Some(&paths), zone), wake)
    }

    pub(crate) fn with_reader(
        mut read: impl FnMut(&mut UsageLimitsView) -> Vec<String> + Send + 'static,
        wake: fn(),
    ) -> Self {
        let shared = std::sync::Arc::new((
            std::sync::Mutex::new(DecorationState::default()),
            std::sync::Condvar::new(),
        ));
        let worker = std::sync::Arc::clone(&shared);
        std::thread::spawn(move || {
            loop {
                let (lock, changed) = &*worker;
                let mut state = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while state.pending.is_none() && !state.closed {
                    state = changed
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                if state.closed {
                    return;
                }
                let Some((generation, mut view)) = state.pending.take() else {
                    continue;
                };
                #[cfg(test)]
                {
                    state.reading = true;
                }
                drop(state);
                let notes = read(&mut view);
                let mut state = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                #[cfg(test)]
                {
                    state.reading = false;
                    changed.notify_all();
                }
                if state.closed {
                    return;
                }
                if state.generation != generation {
                    continue;
                }
                state.ready = Some((generation, view, notes));
                changed.notify_all();
                drop(state);
                wake();
            }
        });
        Self { shared }
    }

    pub(crate) fn request(&mut self, view: UsageLimitsView) {
        let (lock, changed) = &*self.shared;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.generation = state.generation.wrapping_add(1);
        state.pending = Some((state.generation, view));
        state.ready = None;
        changed.notify_one();
    }

    pub(crate) fn invalidate(&mut self) {
        let mut state = self
            .shared
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.generation = state.generation.wrapping_add(1);
        state.pending = None;
        state.ready = None;
    }

    pub(crate) fn poll(&mut self) -> Option<(UsageLimitsView, Vec<String>)> {
        self.shared
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ready
            .take()
            .map(|(_, view, notes)| (view, notes))
    }

    #[cfg(test)]
    pub(crate) fn wait_idle(&self) {
        let (lock, changed) = &*self.shared;
        let state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = changed
            .wait_timeout_while(state, std::time::Duration::from_secs(5), |state| {
                state.reading || state.pending.is_some()
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !result.1.timed_out(),
            "usage decoration did not become idle"
        );
    }

    #[cfg(test)]
    pub(crate) fn wait(&self) {
        let (lock, changed) = &*self.shared;
        let state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = changed
            .wait_timeout_while(state, std::time::Duration::from_secs(5), |state| {
                state.ready.is_none()
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!result.1.timed_out(), "usage decoration did not finish");
    }
}

impl Drop for UsageDecorationReader {
    fn drop(&mut self) {
        let (lock, changed) = &*self.shared;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.pending = None;
        changed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(marker: u64) -> UsageLimitsView {
        UsageLimitsView {
            verticals: Vec::new(),
            session: plakous_host_protocol::views::presentation::SessionLedgerView::default(),
            stale_age_seconds: Some(marker),
        }
    }

    fn latest_read(ready_before_request: bool) {
        let (entered, seen) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let mut reader = UsageDecorationReader::with_reader(
            move |view| {
                let marker = view.stale_age_seconds.unwrap();
                entered.send(marker).unwrap();
                blocked.recv().unwrap();
                vec![marker.to_string()]
            },
            || {},
        );
        reader.request(view(1));
        assert_eq!(
            seen.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            1
        );
        if ready_before_request {
            release.send(()).unwrap();
            reader.wait();
        }
        reader.request(view(2));
        if !ready_before_request {
            release.send(()).unwrap();
        }
        assert_eq!(
            seen.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            2
        );
        assert_eq!(
            reader.poll(),
            None,
            "A must not remain publishable after B was requested"
        );
        release.send(()).unwrap();
        reader.wait();
        assert_eq!(reader.poll(), Some((view(2), vec!["2".into()])));
    }

    #[test]
    fn in_flight_read_is_superseded_before_its_completion() {
        latest_read(false);
    }

    #[test]
    fn completed_ready_slot_is_retired_when_another_read_is_requested() {
        latest_read(true);
    }
}
