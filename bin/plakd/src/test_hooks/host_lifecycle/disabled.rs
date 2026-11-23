//! No-op lifecycle probes without test support.

pub(crate) const fn after_child_handoffs() {}

pub(crate) const fn before_paired_exec() {}

pub(crate) const fn continuation_scan_blocked() -> bool {
    false
}

pub(crate) const fn directory_revalidation(default: std::time::Duration) -> std::time::Duration {
    default
}

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
