//! The explicit starter's lifetime, never a reason to evict a session.

use std::time::{Duration, Instant};

pub(super) const IDLE_EXIT: Duration = Duration::from_secs(300);

#[derive(Default)]
pub(super) struct IdleExit {
    empty_since: Option<Instant>,
}

impl IdleExit {
    pub(super) fn activity(&mut self) {
        self.empty_since = None;
    }

    pub(super) fn expired(&mut self, now: Instant, blocked: bool) -> bool {
        if blocked {
            self.activity();
            return false;
        }
        now.duration_since(*self.empty_since.get_or_insert(now)) >= IDLE_EXIT
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
