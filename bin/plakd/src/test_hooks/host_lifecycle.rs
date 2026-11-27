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

/// Holds the start's inventories until the release file exists or the host
/// stops, after announcing the hold beside it, so a fixture can act while
/// they wait.
pub(crate) fn startup_held(stopped: &dyn Fn() -> bool) {
    hold("PLAK_TEST_HOST_STARTUP_HOLD", stopped);
}

/// Holds the session directory's first projection the same way.
pub(crate) fn directory_projection_held() {
    hold("PLAK_TEST_DIRECTORY_PROJECTION_HOLD", &|| false);
}

fn hold(variable: &str, stopped: &dyn Fn() -> bool) {
    let Some(release) = super::hook_environment(variable) else {
        return;
    };
    let release = std::path::PathBuf::from(release);
    let mut held = release.clone().into_os_string();
    held.push(".held");
    let _ = std::fs::write(held, []);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !release.exists() && !stopped() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A startup inventory failure the fixture names.
pub(crate) fn startup_failure() -> Option<String> {
    super::hook_environment("PLAK_TEST_HOST_STARTUP_FAILURE")
        .map(|value| value.to_string_lossy().into_owned())
}
