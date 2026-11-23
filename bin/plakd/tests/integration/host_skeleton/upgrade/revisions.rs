use super::*;

#[test]
fn revision_forward_qualification_uses_scratch_then_handoff_keeps_the_session() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let session = seed(&fixture, 42);
    let mut source = start_env(
        &fixture,
        &[],
        &[(
            "PLAK_TEST_STORE_REVISION_IMAGE",
            image.to_str().unwrap().to_owned(),
        )],
    );
    // Check the destination's actual probe while the live source owns the home.
    let scratch = tempfile::tempdir_in(fixture.home()).unwrap();
    let service = scratch.path().join("service.json");
    write_private(
        &service,
        &json!({"arguments":["host"],"placements":[]}).to_string(),
    );
    let command = || {
        let mut command = image_command(&fixture, &image);
        command.env("PLAK_TEST_STORE_REVISION_IMAGE", &image);
        command
    };
    let staged = check_legacy_and_stage(&command, scratch.path(), &service);
    let stores = scratch.path().join("stores");
    assert_pending_read_refusal(&mut command());
    for (file, fixture_index) in [
        ("product.sqlite3", "product_revision_fixture"),
        ("subagents.sqlite3", "subagents_revision_fixture"),
    ] {
        assert!(has_index(&stores.join(file), fixture_index), "{file}");
        assert!(
            !has_index(&paths.state_dir().join(file), fixture_index),
            "{file}"
        );
    }
    let probe = command()
        .args([
            "version",
            "--host-upgrade-reopen",
            "--reopen-probe",
            &session,
        ])
        .arg("--store-probe")
        .arg(stores)
        .output()
        .unwrap();
    assert!(probe.status.success(), "{probe:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&probe.stdout).unwrap(),
        json!({"verdict":"opens"})
    );
    assert_eq!(
        Client::connect(&source).request(1, Op::Ping),
        Outcome::Ok(Answer::Pong)
    );

    let capability: serde_json::Value = serde_json::from_slice(&staged.stdout).unwrap();
    let id = capability["binary_id"].as_str().unwrap();
    let upgraded = command()
        .args(["host", "upgrade", "--expect-binary", id])
        .output()
        .unwrap();
    assert!(upgraded.status.success(), "{upgraded:?}");
    assert!(
        Client::connect_at(&source.socket)
            .rows(1)
            .iter()
            .any(|row| row.session == session)
    );
    assert!(has_index(
        &paths.product_database(),
        "product_revision_fixture"
    ));
    assert!(has_index(
        &paths.subagents_database(),
        "subagents_revision_fixture"
    ));
    assert!(
        paths
            .state_dir()
            .join("product.pre-revision-39.sqlite3")
            .exists()
    );
    assert!(
        paths
            .state_dir()
            .join("subagents.pre-revision-12.sqlite3")
            .exists()
    );
    assert!(!fixture.home().join("host/upgrades").exists());
    stop(&mut source);
    assert_fresh_fingerprints(&paths);
}

fn check_legacy_and_stage(
    command: &impl Fn() -> std::process::Command,
    scratch: &std::path::Path,
    service: &std::path::Path,
) -> std::process::Output {
    let probe = || {
        command()
            .args(["version", "--host-upgrade-probe", "--host-upgrade-service"])
            .arg(service)
            .output()
            .unwrap()
    };
    let legacy = probe();
    assert!(!legacy.status.success(), "{legacy:?}");
    assert!(
        String::from_utf8_lossy(&legacy.stderr)
            .contains("the source cannot qualify store revisions"),
        "{legacy:?}"
    );
    assert!(!scratch.join("stores").exists());

    write_private(&scratch.join("scratch-request"), "");
    let staged = probe();
    assert!(staged.status.success(), "{staged:?}");
    staged
}

fn assert_pending_read_refusal(command: &mut std::process::Command) {
    let refused = command
        .args(["session", "check-binding", "--all"])
        .output()
        .unwrap();
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("product store upgrade pending"),
        "{refused:?}"
    );
}

fn assert_fresh_fingerprints(upgraded: &plakd::home::paths::PlakPaths) {
    let Some(fresh) = fixture() else { return };
    let image = PathBuf::from(env!("CARGO_BIN_EXE_plakd"))
        .canonicalize()
        .unwrap();
    let mut host = start_env(
        &fresh,
        &[],
        &[(
            "PLAK_TEST_STORE_REVISION_IMAGE",
            image.to_str().unwrap().to_owned(),
        )],
    );
    stop(&mut host);
    let fresh = plakd::home::paths::PlakPaths::explicit(fresh.home());
    for (db, name) in [
        ("product.sqlite3", "product_schema"),
        ("subagents.sqlite3", "subagents_schema"),
    ] {
        assert_eq!(
            fingerprint(&fresh.state_dir().join(db), name),
            fingerprint(&upgraded.state_dir().join(db), name),
        );
    }
}

fn fingerprint(path: &std::path::Path, identity: &str) -> (i64, String) {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    let fingerprint = connection
        .query_row(&format!("SELECT fingerprint FROM {identity}"), [], |row| {
            row.get(0)
        })
        .unwrap();
    (version, fingerprint)
}

fn has_index(path: &std::path::Path, name: &str) -> bool {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='index' AND name=?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
}
