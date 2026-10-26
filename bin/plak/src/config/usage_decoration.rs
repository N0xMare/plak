//! Latest-only background reads for terminal usage decoration.

use crate::home::paths::PlakPaths;
use crate::product_subscription_usage::annotate_limits;
use plakous_host_protocol::views::presentation::UsageLimitsView;

/// One latest-only display reader; neither mailbox lock is held across I/O.
pub(crate) struct UsageDecorationReader {
    shared: std::sync::Arc<(std::sync::Mutex<DecorationState>, std::sync::Condvar)>,
    generation: u64,
}

#[derive(Default)]
struct DecorationState {
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
                drop(state);
                let notes = read(&mut view);
                let mut state = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.closed {
                    return;
                }
                state.ready = Some((generation, view, notes));
                changed.notify_all();
                drop(state);
                wake();
            }
        });
        Self {
            shared,
            generation: 0,
        }
    }

    pub(crate) fn request(&mut self, view: UsageLimitsView) {
        self.generation = self.generation.wrapping_add(1);
        let (lock, changed) = &*self.shared;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending = Some((self.generation, view));
        state.ready = None;
        changed.notify_one();
    }

    pub(crate) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        let mut state = self
            .shared
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            .filter(|(generation, _, _)| *generation == self.generation)
            .map(|(_, view, notes)| (view, notes))
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
