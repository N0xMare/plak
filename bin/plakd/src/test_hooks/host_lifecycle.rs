//! Lifecycle probes used by the host fixtures.

pub(crate) fn after_child_handoffs() {
    use plakous_hosted::test_support_runner::{HostCut, cut};
    cut(HostCut::AfterChildHandoffs);
}

pub(crate) fn before_paired_exec() {
    let Some(gate) = super::hook_environment("PLAK_TEST_HOST_BEFORE_PAIRED_EXEC") else {
        return;
    };
    let gate = std::path::Path::new(&gate);
    if gate.exists() {
        eprintln!("plakd host: waiting at paired exec gate");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while gate.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

pub(crate) fn continuation_scan_blocked() -> bool {
    super::hook_environment("PLAK_TEST_HOST_CONTINUATION_SCAN_GATE")
        .is_some_and(|path| std::path::Path::new(&path).exists())
}

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

pub(crate) fn record_ready_order(restarting: bool) {
    if let Some(path) = super::hook_environment("PLAK_TEST_HOST_UPGRADE_READY_ORDER") {
        let _ = std::fs::write(path, if restarting { "restarting" } else { "missing" });
    }
}
