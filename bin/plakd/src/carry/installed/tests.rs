use super::*;
use crate::carry::document::IMPORTABLE_VERSIONS;
use std::fs;
use std::time::{Duration, Instant};

#[test]
fn timed_out_reload_probe_does_not_launch_a_fallback_process() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("plakd");
    let calls = root.path().join("calls");
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\necho called >> '{}'\nexec sleep 30\n",
            calls.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let target = probe_reload_target_with_timeout(executable, root.path(), Duration::from_secs(2));
    assert!(target.probe.is_none());
    assert!(target.state_key.is_none());
    assert_eq!(fs::read_to_string(calls).unwrap(), "called\n");
}

#[test]
fn only_a_bare_state_key_is_an_answer() {
    assert!(is_state_key("r20.e16.p25.t2"));
    assert!(!is_state_key(""));
    assert!(!is_state_key("plakd 0.1.0 (release; binary abc)"));
    assert!(!is_state_key("../state"));
}

#[test]
fn a_snapshot_that_hangs_is_killed_and_reads_as_unknown() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let slow = root.path().join("plakd");
    fs::write(&slow, "#!/bin/sh\nexec sleep 30\n").unwrap();
    fs::set_permissions(&slow, fs::Permissions::from_mode(0o700)).unwrap();
    let started = Instant::now();
    let mut command = std::process::Command::new(&slow);
    assert!(
        run_bounded(&mut command, Duration::from_millis(200))
            .unwrap()
            .is_none()
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn reload_probe_checks_config_and_requires_offline_migration_for_changed_keys() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("plakd");
    let key = crate::home::state::state_key();
    let probe = ReloadProbe {
        state_key: key.clone(),
        export_versions: IMPORTABLE_VERSIONS.to_vec(),
        config_error: Some("config.toml: unknown key; run `plakd config edit`".into()),
        binary_id: None,
    };
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s' '{}'\n",
            serde_json::to_string(&probe).unwrap()
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let mut target = probe_reload_target(executable, root.path());
    assert_eq!(target.state_key.as_deref(), Some(key.as_str()));
    let refusal = target.refusal(&key).unwrap();
    assert!(refusal.contains("plakd config edit"));
    assert!(refusal.contains("retry the host upgrade"));
    target.probe.as_mut().unwrap().config_error = None;
    assert!(target.refusal(&key).is_none());
    target.probe.as_mut().unwrap().export_versions = vec![VERSION + 1];
    assert!(target.refusal(&key).is_none());
    assert_eq!(
        target.refusal("different-key").as_deref(),
        Some(crate::carry::upgrade::OFFLINE_MIGRATION_REQUIRED)
    );
}

#[test]
fn reload_probe_falls_back_to_the_old_state_key_answer() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("plakd");
    let key = crate::home::state::state_key();
    fs::write(
        &executable,
        format!("#!/bin/sh\n[ \"$2\" = --state-key ] || exit 2\necho {key}\n"),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let target = probe_reload_target(executable, root.path());
    assert_eq!(target.state_key.as_deref(), Some(key.as_str()));
    assert!(target.probe.is_none());
    assert!(target.refusal(&key).is_none());
}

#[test]
fn a_failed_carry_probe_names_this_builds_export_version_or_the_route_refusal() {
    assert_eq!(
        carry_probe_failure(Some(2), "unknown flag --carry-probe"),
        format!("destination build too old for v{VERSION} carry: unknown flag --carry-probe")
    );
    let refused = carry_probe_failure(Some(1), "no usable login");
    assert!(refused.starts_with("destination route capacity inspection failed: no usable login"));
    assert!(refused.ends_with("retry the host upgrade"));
    assert!(!refused.contains("too old"));
}

/// The host's probe refuses work in flight only for a reload that carries
/// into another session; a reload resuming this session hands it off.
#[test]
fn work_in_flight_refuses_only_a_carrying_reload() {
    let target = |state_key: &str, binding_change: Option<ReopenProbe>| ReloadTarget {
        executable: PathBuf::from("new-installed-plakd"),
        capacity: None,
        capacity_error: None,
        state_key: Some(state_key.to_owned()),
        probe: None,
        reopen: binding_change.map(Box::new),
        request: None,
    };
    let running = "r27.e35.p36.t10";
    for in_flight in [
        crate::carry::CARRY_REFUSED_TURN,
        crate::carry::CARRY_REFUSED_CHILDREN,
    ] {
        assert_eq!(
            reload_refusal(&target(running, None), running, Some(in_flight)),
            None
        );
        assert_eq!(
            reload_refusal(
                &target(
                    running,
                    Some(ReopenProbe::BindingChanged {
                        change: "model".into()
                    })
                ),
                running,
                Some(in_flight)
            )
            .as_deref(),
            Some(in_flight)
        );
    }
    assert_eq!(
        reload_refusal(&target("r99.e99.p99.t99", None), running, None).as_deref(),
        Some(crate::carry::upgrade::OFFLINE_MIGRATION_REQUIRED)
    );
}
