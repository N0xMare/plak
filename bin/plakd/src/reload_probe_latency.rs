//! Shared test-hook latency at the executable reload-probe boundary.

/// Only test hooks install this simulated executable-launch latency.
pub(crate) static DELAY: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
