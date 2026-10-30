//! Execution limits and their defaults.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{ExecutionEnvironmentError, ExecutionEnvironmentErrorKind};

/// Maximum bytes accepted in a portable environment or authority identifier.
pub const MAX_ENVIRONMENT_NAME_BYTES: usize = 128;

/// Maximum entries accepted in either explicit delegation list.
pub const MAX_ENVIRONMENT_ALLOWLIST_ENTRIES: usize = 256;

/// Maximum configured bytes for any individual or aggregate output limit.
pub const MAX_ENVIRONMENT_OUTPUT_BYTES: u64 = 1_073_741_824;

/// Maximum configured execution duration.
pub const MAX_ENVIRONMENT_EXECUTION_DURATION: Duration = Duration::from_secs(86_400);

/// Maximum configured cleanup grace duration.
pub const MAX_ENVIRONMENT_CLEANUP_DURATION: Duration = Duration::from_secs(3_600);

const DEFAULT_STDOUT_BYTES: u64 = 1_048_576;

const DEFAULT_STDERR_BYTES: u64 = 262_144;

const DEFAULT_COMBINED_OUTPUT_BYTES: u64 = DEFAULT_STDOUT_BYTES + DEFAULT_STDERR_BYTES;

const DEFAULT_ARTIFACT_BYTES: u64 = 16_777_216;

const DEFAULT_EXECUTION_DURATION: Duration = Duration::from_secs(300);

// A joined process session outlives one foreground tool call: a polled build or
// test run keeps going between model rounds. Its own ceiling is the session
// duration; the foreground tool's `timeout` ends a joined call earlier.
const DEFAULT_SESSION_DURATION: Duration = Duration::from_secs(36_000);

const DEFAULT_CLEANUP_DURATION: Duration = Duration::from_secs(10);

/// Hard output and deadline limits carried by an environment identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionEnvironmentLimits {
    pub(super) max_stdout_bytes: u64,
    pub(super) max_stderr_bytes: u64,
    pub(super) max_combined_output_bytes: u64,
    pub(super) max_artifact_bytes: u64,
    pub(super) execution_duration: Duration,
    pub(super) session_duration: Duration,
    pub(super) foreground_seconds: u64,
    pub(super) cleanup_duration: Duration,
}

impl ExecutionEnvironmentLimits {
    /// Creates validated output and deadline limits. The joined-session
    /// duration starts equal to `execution_duration`; see
    /// [`Self::with_session_duration`].
    ///
    /// Each limit must be positive. The aggregate output limit must be at least
    /// each stream limit and no greater than their sum. Durations must be finite
    /// positive values within the exported hard maxima.
    ///
    /// # Errors
    ///
    /// Returns an invalid-limit error when limits are contradictory or outside
    /// hard bounds.
    pub fn new(
        max_stdout_bytes: u64,
        max_stderr_bytes: u64,
        max_combined_output_bytes: u64,
        max_artifact_bytes: u64,
        execution_duration: Duration,
        cleanup_duration: Duration,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let output_values = [
            max_stdout_bytes,
            max_stderr_bytes,
            max_combined_output_bytes,
            max_artifact_bytes,
        ];
        let output_valid = output_values
            .iter()
            .all(|value| (1..=MAX_ENVIRONMENT_OUTPUT_BYTES).contains(value));
        let stream_sum = max_stdout_bytes.saturating_add(max_stderr_bytes);
        let aggregate_valid = max_combined_output_bytes >= max_stdout_bytes
            && max_combined_output_bytes >= max_stderr_bytes
            && max_combined_output_bytes <= stream_sum;
        let deadlines_valid =
            duration_in_range(execution_duration, MAX_ENVIRONMENT_EXECUTION_DURATION)
                && duration_in_range(cleanup_duration, MAX_ENVIRONMENT_CLEANUP_DURATION);
        if !output_valid || !aggregate_valid || !deadlines_valid {
            return Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::InvalidLimit,
                "execution environment limits are invalid",
            ));
        }
        Ok(Self {
            max_stdout_bytes,
            max_stderr_bytes,
            max_combined_output_bytes,
            max_artifact_bytes,
            execution_duration,
            session_duration: execution_duration,
            foreground_seconds: 30,
            cleanup_duration,
        })
    }

    /// Replaces the joined process-session duration: the ceiling for one
    /// owned process session, which a polled command keeps running across
    /// model rounds. Literal processes keep `execution_duration`.
    ///
    /// # Errors
    ///
    /// Rejects a duration shorter than `execution_duration` or above
    /// [`MAX_ENVIRONMENT_EXECUTION_DURATION`].
    pub fn with_session_duration(
        mut self,
        session_duration: Duration,
    ) -> Result<Self, ExecutionEnvironmentError> {
        if session_duration < self.execution_duration
            || !duration_in_range(session_duration, MAX_ENVIRONMENT_EXECUTION_DURATION)
        {
            return Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::InvalidLimit,
                "execution environment session duration is invalid",
            ));
        }
        self.session_duration = session_duration;
        Ok(self)
    }

    /// Binds the default process observation budget into environment identity.
    ///
    /// # Errors
    /// Rejects budgets outside 1..=600 seconds.
    pub fn with_foreground_seconds(
        mut self,
        seconds: u64,
    ) -> Result<Self, ExecutionEnvironmentError> {
        if !(1..=600).contains(&seconds) {
            return Err(ExecutionEnvironmentError::new(
                ExecutionEnvironmentErrorKind::InvalidLimit,
                "foreground process budget is invalid",
            ));
        }
        self.foreground_seconds = seconds;
        Ok(self)
    }
    /// Returns the construction-bound default process hint in seconds.
    #[must_use]
    pub const fn foreground_seconds(self) -> u64 {
        self.foreground_seconds
    }

    /// Returns the per-operation stdout byte cap.
    #[must_use]
    pub const fn max_stdout_bytes(self) -> u64 {
        self.max_stdout_bytes
    }

    /// Returns the per-operation stderr byte cap.
    #[must_use]
    pub const fn max_stderr_bytes(self) -> u64 {
        self.max_stderr_bytes
    }

    /// Returns the aggregate stdout-plus-stderr byte cap.
    #[must_use]
    pub const fn max_combined_output_bytes(self) -> u64 {
        self.max_combined_output_bytes
    }

    /// Returns the per-artifact byte cap.
    #[must_use]
    pub const fn max_artifact_bytes(self) -> u64 {
        self.max_artifact_bytes
    }

    /// Returns the maximum execution duration.
    #[must_use]
    pub const fn execution_duration(self) -> Duration {
        self.execution_duration
    }

    /// Returns the maximum lifetime of one joined process session.
    #[must_use]
    pub const fn session_duration(self) -> Duration {
        self.session_duration
    }

    /// Returns the cleanup grace duration.
    #[must_use]
    pub const fn cleanup_duration(self) -> Duration {
        self.cleanup_duration
    }
}

impl Default for ExecutionEnvironmentLimits {
    fn default() -> Self {
        Self {
            max_stdout_bytes: DEFAULT_STDOUT_BYTES,
            max_stderr_bytes: DEFAULT_STDERR_BYTES,
            max_combined_output_bytes: DEFAULT_COMBINED_OUTPUT_BYTES,
            max_artifact_bytes: DEFAULT_ARTIFACT_BYTES,
            execution_duration: DEFAULT_EXECUTION_DURATION,
            session_duration: DEFAULT_SESSION_DURATION,
            foreground_seconds: 30,
            cleanup_duration: DEFAULT_CLEANUP_DURATION,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecutionEnvironmentLimitsWire {
    pub(super) max_stdout_bytes: u64,
    pub(super) max_stderr_bytes: u64,
    pub(super) max_combined_output_bytes: u64,
    pub(super) max_artifact_bytes: u64,
    pub(super) execution_seconds: u64,
    pub(super) execution_nanoseconds: u32,
    pub(super) session_seconds: u64,
    pub(super) foreground_seconds: u64,
    pub(super) session_nanoseconds: u32,
    pub(super) cleanup_seconds: u64,
    pub(super) cleanup_nanoseconds: u32,
}

fn duration_in_range(duration: Duration, maximum: Duration) -> bool {
    !duration.is_zero() && duration <= maximum
}
