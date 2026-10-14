//! Pure RFC 8628-style device poll schedule math.

use std::time::{Duration, Instant};

use super::OAuthWorkflowError;

/// Bounded device-authorization poll schedule.
///
/// Owns only interval, clamp, slow-down, and deadline arithmetic. HTTP and
/// token exchange remain in dialect managers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DevicePollSchedule {
    interval: Duration,
    max_interval: Duration,
    slow_down_increment: Duration,
    deadline: Instant,
}

impl DevicePollSchedule {
    /// Creates a schedule with a clamped positive interval.
    ///
    /// # Errors
    ///
    /// Returns [`OAuthWorkflowError::InvalidForm`] when `interval` is zero or
    /// greater than `max_interval`, or when `max_interval` is zero.
    pub fn new(
        interval: Duration,
        max_interval: Duration,
        slow_down_increment: Duration,
        deadline: Instant,
    ) -> Result<Self, OAuthWorkflowError> {
        if max_interval.is_zero() || interval.is_zero() || interval > max_interval {
            return Err(OAuthWorkflowError::InvalidForm);
        }
        Ok(Self {
            interval,
            max_interval,
            slow_down_increment,
            deadline,
        })
    }

    /// Creates a schedule from whole-second provider interval values.
    ///
    /// # Errors
    ///
    /// Same as [`Self::new`].
    pub fn from_seconds(
        interval_secs: u64,
        max_interval_secs: u64,
        slow_down_increment: Duration,
        deadline: Instant,
    ) -> Result<Self, OAuthWorkflowError> {
        Self::new(
            Duration::from_secs(interval_secs),
            Duration::from_secs(max_interval_secs),
            slow_down_increment,
            deadline,
        )
    }

    /// Returns the current poll interval.
    #[must_use]
    pub const fn interval(&self) -> Duration {
        self.interval
    }

    /// Returns the maximum allowed interval after slow-down clamping.
    #[must_use]
    pub const fn max_interval(&self) -> Duration {
        self.max_interval
    }

    /// Returns the flow deadline.
    #[must_use]
    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Returns time remaining until the deadline at `now`.
    #[must_use]
    pub fn remaining(&self, now: Instant) -> Duration {
        self.deadline.saturating_duration_since(now)
    }

    /// Returns whether the deadline has been reached at `now`.
    #[must_use]
    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }

    /// Returns how long a poller should sleep before the next attempt.
    ///
    /// This is `interval` clamped to remaining deadline time (may be zero).
    #[must_use]
    pub fn sleep_for(&self, now: Instant) -> Duration {
        self.interval.min(self.remaining(now))
    }

    /// Applies an RFC 8628 `slow_down` increment, clamped to `max_interval`.
    pub fn apply_slow_down(&mut self) {
        self.interval = self
            .interval
            .saturating_add(self.slow_down_increment)
            .min(self.max_interval);
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::DevicePollSchedule;
    use crate::oauth_workflow::OAuthWorkflowError;
    use std::time::{Duration, Instant};

    #[test]
    fn clamps_interval_and_remaining_sleep() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(12);
        let schedule = DevicePollSchedule::from_seconds(5, 60, Duration::from_secs(5), deadline)
            .expect("schedule");
        assert_eq!(schedule.interval(), Duration::from_secs(5));
        assert_eq!(schedule.remaining(now), Duration::from_secs(12));
        assert_eq!(schedule.sleep_for(now), Duration::from_secs(5));
        assert_eq!(
            schedule.sleep_for(now + Duration::from_secs(10)),
            Duration::from_secs(2)
        );
        assert!(!schedule.is_expired(now));
        assert!(schedule.is_expired(deadline));
        assert_eq!(
            schedule.remaining(deadline + Duration::from_secs(1)),
            Duration::ZERO
        );
    }

    #[test]
    fn slow_down_increments_and_clamps() {
        let now = Instant::now();
        let mut schedule = DevicePollSchedule::new(
            Duration::from_secs(5),
            Duration::from_secs(12),
            Duration::from_secs(5),
            now + Duration::from_secs(60),
        )
        .expect("schedule");
        schedule.apply_slow_down();
        assert_eq!(schedule.interval(), Duration::from_secs(10));
        schedule.apply_slow_down();
        assert_eq!(schedule.interval(), Duration::from_secs(12));
        schedule.apply_slow_down();
        assert_eq!(schedule.interval(), Duration::from_secs(12));
    }

    #[test]
    fn rejects_zero_or_oversized_interval() {
        let deadline = Instant::now() + Duration::from_secs(30);
        assert_eq!(
            DevicePollSchedule::from_seconds(0, 30, Duration::from_secs(5), deadline),
            Err(OAuthWorkflowError::InvalidForm)
        );
        assert_eq!(
            DevicePollSchedule::from_seconds(31, 30, Duration::from_secs(5), deadline),
            Err(OAuthWorkflowError::InvalidForm)
        );
        assert_eq!(
            DevicePollSchedule::new(
                Duration::from_secs(1),
                Duration::ZERO,
                Duration::from_secs(1),
                deadline,
            ),
            Err(OAuthWorkflowError::InvalidForm)
        );
    }
}
