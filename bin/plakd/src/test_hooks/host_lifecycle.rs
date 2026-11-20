//! No-op lifecycle probes when `test-hooks` is disabled.

pub(crate) const fn bridge_discovery_window(default: std::time::Duration) -> std::time::Duration {
    default
}

pub(crate) const fn bridge_start_blocked() {}

pub(crate) fn record_handoff_budget(
    _elapsed: std::time::Duration,
    _remaining: std::time::Duration,
) {
}

pub(crate) fn record_ready_order(_restarting: bool) {}
