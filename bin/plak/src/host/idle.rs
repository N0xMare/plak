//! The explicit starter's lifetime, never a reason to evict a session.

use std::time::{Duration, Instant};

pub(super) const IDLE_EXIT: Duration = Duration::from_secs(300);

pub(super) struct IdleExit {
    empty_since: Option<Instant>,
    window: Duration,
}

impl Default for IdleExit {
    fn default() -> Self {
        Self {
            empty_since: None,
            window: IDLE_EXIT,
        }
    }
}

impl IdleExit {
    #[cfg(test)]
    pub(super) fn with_window(window: Duration) -> Self {
        Self {
            empty_since: None,
            window,
        }
    }

    pub(super) fn activity(&mut self) {
        self.empty_since = None;
    }

    pub(super) fn expired(&mut self, now: Instant, blocked: bool) -> bool {
        if blocked {
            self.activity();
            return false;
        }
        now.duration_since(*self.empty_since.get_or_insert(now)) >= self.window
    }
}

/// Durable reads are never polled on the accept loop until their task is ready.
pub(super) struct IdleGuard {
    clock: IdleExit,
    next_check: Instant,
    checking: Option<tokio::task::JoinHandle<bool>>,
    blocked: bool,
}

impl IdleGuard {
    pub(super) fn new(clock: IdleExit) -> Self {
        Self {
            clock,
            next_check: Instant::now(),
            checking: None,
            blocked: true,
        }
    }

    pub(super) fn activity(&mut self) {
        self.clock.activity();
    }

    pub(super) async fn expired(
        &mut self,
        now: Instant,
        blocked: bool,
        host: &std::sync::Arc<
            plakous_hosted::host_service::wire::SessionHost<crate::surface_protocol::PlakExtension>,
        >,
    ) -> bool {
        let fresh = self
            .checking
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished);
        if fresh && let Some(check) = self.checking.take() {
            self.blocked = check.await.unwrap_or(true);
            self.next_check = now + Duration::from_secs(30);
        }
        let expired = self.clock.expired(now, blocked || self.blocked);
        if expired && fresh {
            return true;
        }
        if self.checking.is_none() && (expired || now >= self.next_check) {
            let host = std::sync::Arc::clone(host);
            self.checking = Some(tokio::task::spawn_blocking(move || {
                host.durable_idle_exit_blocked()
            }));
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_and_kept_work_restart_the_entire_five_minute_window() {
        let start = Instant::now();
        let mut idle = IdleExit::default();
        assert!(!idle.expired(start, false));
        assert!(!idle.expired(start + Duration::from_secs(299), false));
        assert!(!idle.expired(start + IDLE_EXIT, true));
        assert!(!idle.expired(start + IDLE_EXIT * 2, false));
        idle.activity();
        assert!(!idle.expired(start + IDLE_EXIT * 3, false));
        assert!(idle.expired(start + IDLE_EXIT * 4, false));
    }
}
