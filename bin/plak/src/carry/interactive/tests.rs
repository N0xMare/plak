use super::*;
use crate::carry::document::{IMPORTABLE_VERSIONS, read_document, sample_document};
use crate::home::paths::PlakPaths;
use std::fs;
use std::time::{Duration, Instant};

#[test]
fn timed_out_reload_probe_does_not_launch_a_fallback_process() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("plak");
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
    assert!(!is_state_key("plak 0.1.0 (release; binary abc)"));
    assert!(!is_state_key("../state"));
}

#[test]
fn a_snapshot_that_hangs_is_killed_and_reads_as_unknown() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let slow = root.path().join("plak");
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
fn reload_resumes_plainly_on_the_same_key_and_carries_across_keys() {
    assert_eq!(reload_plan(false, "a", Some("b")), ReloadPlan::Fresh);
    assert_eq!(reload_plan(true, "a", Some("a")), ReloadPlan::Resume);
    assert_eq!(reload_plan(true, "a", Some("b")), ReloadPlan::Carry);
    assert_eq!(reload_plan(true, "a", None), ReloadPlan::ResumeUnverified);
}

#[test]
fn reload_probe_checks_config_and_carry_versions_before_handoff() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("plak");
    let key = crate::home::state::state_key();
    let probe = ReloadProbe {
        state_key: key.clone(),
        export_versions: IMPORTABLE_VERSIONS.to_vec(),
        config_error: Some("config.toml: unknown key; run `plak config edit`".into()),
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
    for running in [&key, "different-key"] {
        let refusal = target.refusal(running).unwrap();
        assert!(refusal.contains("plak config edit"));
        assert!(refusal.contains("retry /reload"));
    }
    target.probe.as_mut().unwrap().config_error = None;
    assert!(target.refusal(&key).is_none());
    target.probe.as_mut().unwrap().export_versions = vec![VERSION + 1];
    assert!(target.refusal(&key).is_none());
    assert!(
        target
            .refusal("different-key")
            .unwrap()
            .contains("cannot import")
    );
}

#[test]
fn reload_probe_falls_back_to_the_old_state_key_answer() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("plak");
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
fn snapshot_export_version_manifest_treats_missing_and_unparseable_as_unknown() {
    let root = tempfile::tempdir().unwrap();
    let snapshot = root.path().join("plak");
    assert_eq!(snapshot_export_version(&snapshot).unwrap(), None);
    for version in [7, VERSION, 999] {
        fs::write(root.path().join("export-version"), format!("{version}\n")).unwrap();
        assert_eq!(snapshot_export_version(&snapshot).unwrap(), Some(version));
    }
    for manifest in ["", "not a version", "4\n5", "4294967296"] {
        fs::write(root.path().join("export-version"), manifest).unwrap();
        assert_eq!(snapshot_export_version(&snapshot).unwrap(), None);
    }
}

#[test]
fn the_newest_private_installed_snapshot_for_a_key_is_chosen() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let snapshot = |name: &str, key: &str| {
        let dir = root.path().join(name);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("state-key"), format!("{key}\n")).unwrap();
        fs::write(dir.join("plak"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("plak"), fs::Permissions::from_mode(0o555)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        dir
    };
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let c = "c".repeat(64);
    snapshot(&a, "r1.e1.p1.t1");
    snapshot(&b, "r2.e2.p2.t2");
    snapshot(&c, "r1.e1.p1.t1");
    // Newer, but not an installed snapshot or not private: never chosen.
    snapshot(".staging.abcdef", "r1.e1.p1.t1");
    snapshot("not-a-snapshot", "r1.e1.p1.t1");
    let open = snapshot(&"d".repeat(64), "r1.e1.p1.t1");
    fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
    let writable = snapshot(&"e".repeat(64), "r1.e1.p1.t1");
    fs::set_permissions(writable.join("plak"), fs::Permissions::from_mode(0o757)).unwrap();
    fs::create_dir(root.path().join("f".repeat(64))).unwrap();
    assert_eq!(
        snapshot_for_key(&[root.path().to_path_buf()], "r1.e1.p1.t1"),
        Some(root.path().join(&c).join("plak"))
    );
    assert_eq!(
        snapshot_for_key(&[root.path().to_path_buf()], "r2.e2.p2.t2"),
        Some(root.path().join(&b).join("plak"))
    );
    assert_eq!(
        snapshot_for_key(&[root.path().to_path_buf()], "r9.e9.p9.t9"),
        None
    );
}

#[test]
fn snapshots_under_every_installed_root_are_searched() {
    use std::os::unix::fs::PermissionsExt as _;
    let maintainer = tempfile::tempdir().unwrap();
    let release = tempfile::tempdir().unwrap();
    let snapshot = |root: &Path, name: &str, key: &str| {
        let dir = root.join(name);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("state-key"), format!("{key}\n")).unwrap();
        fs::write(dir.join("plak"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("plak"), fs::Permissions::from_mode(0o555)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        dir.join("plak")
    };
    let roots = [
        maintainer.path().to_path_buf(),
        release.path().to_path_buf(),
    ];
    let only_release = snapshot(release.path(), &"a".repeat(64), "r1.e1.p1.t1");
    assert_eq!(snapshot_for_key(&roots, "r1.e1.p1.t1"), Some(only_release));
    snapshot(release.path(), &"b".repeat(64), "r2.e2.p2.t2");
    let newer_maintainer = snapshot(maintainer.path(), &"c".repeat(64), "r2.e2.p2.t2");
    assert_eq!(
        snapshot_for_key(&roots, "r2.e2.p2.t2"),
        Some(newer_maintainer)
    );
    let missing = [
        maintainer.path().join("missing"),
        release.path().to_path_buf(),
    ];
    assert!(snapshot_for_key(&missing, "r1.e1.p1.t1").is_some());
}

#[test]
fn a_fake_snapshot_answers_its_key_and_exports_through_its_own_cli() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let snapshot = root.path().join("plak");
    fs::write(
        &snapshot,
        "#!/bin/sh\n\
         case \"$1 $2\" in\n\
         'version --state-key') echo r1.e1.p1.t1 ;;\n\
         'session export') [ \"$3\" = good ] || { echo \"no session $3\" >&2; exit 2; }\n\
           pwd > \"$5\" ;;\n\
         *) exit 9 ;;\n\
         esac\n",
    )
    .unwrap();
    fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        installed_state_key(&snapshot).as_deref(),
        Some("r1.e1.p1.t1")
    );
    assert_eq!(installed_state_key(&root.path().join("missing")), None);

    let out = root.path().join("out.json");
    export_with_snapshot(&snapshot, "good", &out, root.path()).unwrap();
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        root.path().canonicalize().unwrap().to_str().unwrap()
    );
    let failure = export_with_snapshot(&snapshot, "bad", &out, root.path()).unwrap_err();
    assert!(
        failure.ends_with("could not export session bad: no session bad"),
        "{failure}"
    );
}

#[test]
fn a_failed_snapshot_export_does_not_leave_a_partial_carry_file() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let snapshot = root.path().join("plak");
    fs::write(&snapshot, "#!/bin/sh\necho partial > \"$5\"\nexit 2\n").unwrap();
    fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o700)).unwrap();
    let out = root.path().join("carry.json");
    assert!(export_with_snapshot(&snapshot, "source", &out, root.path()).is_err());
    assert!(!out.exists());
}

#[test]
fn carry_files_are_private_and_never_overwritten() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(root.path().join("home"));
    fs::create_dir(paths.home()).unwrap();
    let path = new_carry_file(&paths, "5f0e2a43-7d3c-4c1e-9a55-4b1f7f0c9a11").unwrap();
    let dir = path.parent().unwrap();
    assert_eq!(dir, paths.home().join("carry"));
    assert_eq!(
        fs::metadata(dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let document = sample_document();
    write_document(&path, &document).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(write_document(&path, &document).is_err());
    let read = read_document(&path).unwrap();
    assert_eq!(read.source().0, document.source.session_id);
}
