//! Default and maintainer binaries contain no explicit client fixture producer.
#![allow(clippy::expect_used, reason = "binary contract fixture")]
use std::process::Command;

#[test]
fn client_fixture_hooks_are_absent_without_test_hooks() {
    let binary = env!("CARGO_BIN_EXE_plak");
    let bytes = std::fs::read(binary).expect("product binary");
    for marker in [
        "--test-seed-abandoned-open",
        "PLAK_TEST_DRAFT_PENDING_ID",
        "PLAK_TEST_DRAFT_PENDING_READY",
        "PLAK_TEST_DRAFT_PENDING_RELEASE",
        "PLAK_TEST_RECOVERY_FIRST_ATTACH",
    ] {
        assert!(
            !bytes
                .windows(marker.len())
                .any(|window| window == marker.as_bytes()),
            "fixture marker compiled into product: {marker}"
        );
    }
    let output = Command::new(binary)
        .arg("--version")
        .env("PLAK_TEST_DRAFT_PENDING_ID", "invalid")
        .env("PLAK_TEST_DRAFT_PENDING_READY", "invalid")
        .env("PLAK_TEST_DRAFT_PENDING_RELEASE", "invalid")
        .env("PLAK_TEST_RECOVERY_FIRST_ATTACH", "invalid")
        .output()
        .expect("version");
    assert!(output.status.success());
    let output = Command::new(binary)
        .args(["--test-seed-abandoned-open", "without-submission"])
        .output()
        .expect("unknown fixture flag");
    assert!(!output.status.success());
}
