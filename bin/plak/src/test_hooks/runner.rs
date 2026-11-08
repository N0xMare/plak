//! Controlled runner transport and awake-time inputs for real-process proofs.

use std::path::Path;
use std::sync::Arc;

#[allow(dead_code, reason = "only the optional host dials its local runner")]
pub(crate) fn runner_windows() -> plakous::runner::ClientWindows {
    let window = super::selected()
        .and_then(Result::ok)
        .and_then(|selection| selection.plan.runner_window_ms);
    window.map_or_else(plakous::runner::ClientWindows::default, |ms| {
        plakous::runner::ClientWindows {
            read: std::time::Duration::from_millis(ms),
            effect: std::time::Duration::from_millis(ms),
            ping: std::time::Duration::from_millis(100),
            dead: std::time::Duration::from_millis(500),
        }
    })
}

/// Owner-only fixture ticks let strict sweeps distinguish suspend from awake
/// lease lapse without changing the machine clock or waiting five minutes.
pub(crate) fn runner_lease_clock(home: &Path) -> Option<Arc<plakous::runner::RunnerLeaseClock>> {
    let path = home.join("lease-clock-ms");
    if !path.is_file() {
        return None;
    }
    let origin = std::time::Instant::now();
    Some(Arc::new(move || {
        let elapsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(0);
        origin + std::time::Duration::from_millis(elapsed)
    }))
}
