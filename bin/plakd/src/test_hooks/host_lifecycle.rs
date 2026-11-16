//! No-op lifecycle probes when `test-hooks` is disabled.

pub(crate) fn record_handoff_budget(
    _elapsed: std::time::Duration,
    _remaining: std::time::Duration,
) {
}

pub(crate) fn record_ready_order(_restarting: bool) {}
