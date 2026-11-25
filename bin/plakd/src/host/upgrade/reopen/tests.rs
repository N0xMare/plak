use super::*;
use sha2::{Digest as _, Sha256};
use std::os::unix::fs::PermissionsExt as _;

fn probe_image(path: &Path, answer: &serde_json::Value) -> String {
    let script = format!("#!/bin/sh\nprintf '%s\\n' '{answer}'\n");
    std::fs::write(path, &script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    crate::shipping_artifact::hex_digest(&Sha256::digest(script.as_bytes())[..6])
}

#[test]
fn an_unproven_child_comparison_refuses_under_the_holds() {
    let (_root, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    let source = paths.home().join("source-probe");
    let destination = paths.home().join("destination-probe");
    let source_id = probe_image(
        &source,
        &serde_json::json!({
            "opened": ["previous-child-head"], "faults": []
        }),
    );
    let destination_id = probe_image(
        &destination,
        &serde_json::json!({
            "verdict": "opens",
            "child_inspection": {
                "opened": [],
                "faults": [["new-child-head", {"kind": "binding", "detail": "binding changed"}]]
            }
        }),
    );
    let request = Request {
        source_instance: plakous_recursive_subagents::tree::HostInstance::generate().to_string(),
        source_key: crate::home::state::state_key(),
        destination,
        destination_key: crate::home::state::state_key(),
        expected_binary: destination_id,
        drain: crate::command::HostDrain::default(),
        wait_seconds: 0,
        abandon: None,
        inspect_runners: Vec::new(),
    };
    let preflight = Preflight {
        passed: BTreeMap::new(),
        rehearsed: false,
        source: SourceImage {
            verified: process::verify_image(&source, &source_id).unwrap(),
            path: source,
        },
    };
    let roots = [Root {
        id: "root".into(),
        local: true,
        head: None,
    }];
    let progress = |_: progress::Event<'_>| Ok(());
    assert_eq!(
        probe_roots(
            &paths,
            &request,
            None,
            &roots,
            &preflight.source,
            true,
            &progress
        )
        .unwrap(),
        [false]
    );
    let failure = verify_held(&paths, &request, None, &roots, &preflight, &progress).unwrap_err();
    assert!(
        failure
            .message()
            .starts_with("host_upgrade_child_reopen_unproven:")
    );
}
