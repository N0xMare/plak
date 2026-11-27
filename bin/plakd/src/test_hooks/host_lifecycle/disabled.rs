//! No-op lifecycle probes without test support.

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

pub(crate) fn startup_held(_stopped: &dyn Fn() -> bool) {}

pub(crate) const fn directory_projection_held() {}

pub(crate) const fn startup_failure() -> Option<String> {
    None
}
