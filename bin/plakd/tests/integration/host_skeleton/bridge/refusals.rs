//! Refusals are stdout protocol, not stderr parsing or an absence guess.

use super::*;

fn refusal(fixture: &Fixture, reason: &str) {
    let result = output_after_hello(
        fixture.command().args(["host", "connect", "--stdio"]),
        WIRE_VERSION,
    );
    assert_eq!(result.status.code(), Some(1));
    let mut expected = json!({"frame":"refused","kind":"bridge_unavailable","reason":reason});
    if matches!(reason, "absent" | "starting" | "stopping") {
        expected["retry_after_ms"] = json!(500);
    }
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
        expected
    );
}

#[test]
fn bridge_refusal_of_protocol_twelve_does_not_discover_or_start_a_host() {
    let Some(fixture) = fixture() else { return };
    let result = output_after_hello(
        fixture
            .command()
            .args(["host", "connect", "--stdio", "--auto-start"]),
        12,
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
        json!({"frame":"refused","kind":"version_mismatch","host":WIRE_VERSION,"client":12})
    );
    assert!(!fixture.home().join("host/host.lock").exists());
}

#[test]
fn bridge_refusal_requires_one_bounded_hello_before_discovery() {
    let Some(fixture) = fixture() else { return };
    let mut command = fixture.command();
    let mut child = Child::spawn(
        command
            .args(["host", "connect", "--stdio", "--auto-start"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"frame\":\"request\",\"id\":1,\"op\":{\"op\":\"ping\"}}\n")
        .unwrap();
    let result = finished_output(child);
    let frame: HostFrame<PlakExtension> = serde_json::from_slice(&result.stdout).unwrap();
    assert!(matches!(
        frame,
        HostFrame::Refused(plakous_host_protocol::wire::HandshakeRefusal::Malformed { .. })
    ));
    assert!(!fixture.home().join("host/host.lock").exists());
}

#[test]
fn bridge_refusal_auto_start_permission_failure_requires_repair() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = StartedHome(fixture.home());
    let host = fixture.home().join("host");
    std::fs::create_dir_all(&host).unwrap();
    let log = host.join("auto-start.log");
    std::fs::write(&log, "not a private log").unwrap();
    std::fs::set_permissions(log, std::fs::Permissions::from_mode(0o644)).unwrap();
    let result = output_after_hello(
        fixture
            .command()
            .args(["host", "connect", "--stdio", "--auto-start"]),
        WIRE_VERSION,
    );
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
        json!({"frame":"refused","kind":"bridge_unavailable","reason":"verification_failed"})
    );
}

#[test]
fn bridge_refusal_absent_is_typed_without_starting_any_host() {
    let Some(fixture) = fixture() else { return };
    refusal(&fixture, "absent");
    assert!(!fixture.home().join("host/host.lock").exists());
}

#[test]
fn bridge_refusal_verification_failed_does_not_guess_absence() {
    let Some(fixture) = fixture() else { return };
    std::fs::set_permissions(fixture.home(), std::fs::Permissions::from_mode(0o777)).unwrap();
    refusal(&fixture, "verification_failed");
}

fn held_lock(fixture: &Fixture, key: &str, phase: &str) -> std::fs::File {
    let directory = fixture.home().join("state").join(key);
    std::fs::create_dir_all(&directory).unwrap();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(directory.join("host.lock"))
        .unwrap();
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .unwrap();
    file.lock().unwrap();
    writeln!(&file, "{} fixture {phase}", std::process::id()).unwrap();
    file
}

#[test]
fn bridge_refusal_incompatible_state_requires_a_live_kernel_lock() {
    let Some(fixture) = fixture() else { return };
    let held = held_lock(&fixture, "r999.e999.p999.t999", "starting");
    refusal(&fixture, "incompatible_state");
    drop(held);
    refusal(&fixture, "absent");
}

#[test]
fn bridge_refusal_starting_waits_for_readiness_but_stopping_is_typed() {
    let Some(fixture) = fixture() else { return };
    let key = plakd::home::state::state_key();
    let held = held_lock(&fixture, &key, "starting");
    refusal(&fixture, "starting");
    drop(held);
    let held = held_lock(&fixture, &key, "stopping");
    refusal(&fixture, "stopping");
    let result = output_after_hello(
        fixture
            .command()
            .args(["host", "connect", "--stdio", "--auto-start"]),
        WIRE_VERSION,
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
        json!({"frame":"refused","kind":"bridge_unavailable","reason":"stopping","retry_after_ms":500})
    );
    assert!(!fixture.home().join("host/auto-start.log").exists());
    drop(held);
    let held = held_lock(&fixture, &key, "restarting");
    let mut bridge = Bridge::start(&fixture);
    bridge.hello(WIRE_VERSION);
    assert!(
        bridge
            .lines
            .recv_timeout(Duration::from_millis(100))
            .is_err(),
        "restart was refused instead of awaited"
    );
    drop(held);
    let mut host = start(&fixture);
    assert!(matches!(bridge.next(), HostFrame::Welcome(_)));
    bridge.finish();
    stop(&mut host);
}
