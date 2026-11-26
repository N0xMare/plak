//! Lifecycle probes used by the host fixtures.

pub(crate) fn directory_revalidation(default: std::time::Duration) -> std::time::Duration {
    super::hook_environment("PLAK_TEST_DIRECTORY_REVALIDATE_MS")
        .and_then(|value| value.to_str()?.parse::<u64>().ok())
        .filter(|millis| (1..=60_000).contains(millis))
        .map_or(default, std::time::Duration::from_millis)
}

pub(crate) fn bridge_discovery_window(default: std::time::Duration) -> std::time::Duration {
    super::shorter_seconds("PLAK_TEST_BRIDGE_DISCOVERY_SECONDS", default)
}

pub(crate) fn bridge_start_blocked() {
    if let Some(marker) = super::hook_environment("PLAK_TEST_BRIDGE_START_BLOCKED") {
        let _ = std::fs::write(marker, []);
    }
}

pub(crate) fn record_handoff_budget(elapsed: std::time::Duration, remaining: std::time::Duration) {
    if let Some(path) = super::hook_environment("PLAK_TEST_HOST_HANDOFF_BUDGET") {
        let value = serde_json::json!({"elapsed_ms":elapsed.as_millis(), "remaining_ms":remaining.as_millis()});
        let _ = std::fs::write(path, value.to_string());
    }
}
