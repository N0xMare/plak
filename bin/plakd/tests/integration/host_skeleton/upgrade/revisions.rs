use super::*;

#[test]
fn revision_forward_qualification_reads_two_durable_scripted_trees() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    fixture.write_script(&json!({
        "version": 1,
        "tree": true,
        "durable_tree": true,
        "steps": [{"output": {"text": ANSWER}}]
    }));
    let sessions = [(); 2].map(|()| seed_scripted_tree(&fixture));
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let revision_image = "PLAK_TEST_STORE_REVISION_IMAGE";
    let mut source = start_env(
        &fixture,
        &[],
        &[(revision_image, image.to_str().unwrap().to_owned())],
    );
    let scratch = tempfile::tempdir_in(fixture.home()).unwrap();
    let service = scratch.path().join("service.json");
    write_private(
        &service,
        &json!({"arguments":["host"],"placements":[]}).to_string(),
    );
    let command = || {
        let mut command = image_command(&fixture, &image);
        command.env(revision_image, &image);
        command
    };
    check_legacy_and_stage(&command, scratch.path(), &service);
    let stores = scratch.path().join("stores");
    let tree_store = stores.join("subagents.sqlite3");
    assert!(has_index(&tree_store, "subagents_revision_fixture"));
    {
        let copy = rusqlite::Connection::open_with_flags(
            tree_store,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        for session in &sessions {
            let exists: bool = copy
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM agent_trees WHERE tree_id=?1)",
                    [session],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "no durable tree for {session}");
        }
    }
    assert_parallel_inspection(&command, &stores, &sessions);
    stop(&mut source);
}

fn seed_scripted_tree(fixture: &Fixture) -> String {
    let consent = fixture.root.join("tree-seed-consent");
    write_private(&consent, "y\n");
    let run = fixture
        .command()
        .args([
            "run",
            "--account",
            "codex",
            "--model",
            "offline-loopback-v1",
            "--effort",
            "medium",
            "--print-session-id",
            "--",
            "answer once",
        ])
        .stdin(std::fs::File::open(consent).unwrap())
        .output()
        .unwrap();
    assert!(run.status.success(), "{run:?}");
    String::from_utf8(run.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("session_id="))
        .unwrap()
        .to_owned()
}

#[test]
fn revision_forward_qualification_uses_scratch_then_handoff_keeps_the_session() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let sessions = [seed(&fixture, 42), seed(&fixture, 43)];
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
    assert_live_prechecks(&fixture, &command, &paths, &sessions);
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
    assert_parallel_inspection(&command, &stores, &sessions);
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
    let rows = Client::connect_at(&source.socket).rows(1);
    for session in sessions {
        assert!(rows.iter().any(|row| row.session == session));
    }
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
            .join("product.pre-revision-40.sqlite3")
            .exists()
    );
    assert!(
        paths
            .state_dir()
            .join("subagents.pre-revision-13.sqlite3")
            .exists()
    );
    assert!(!fixture.home().join("host/upgrades").exists());
    stop(&mut source);
    assert_fresh_fingerprints(&paths);
}

fn assert_live_prechecks(
    fixture: &Fixture,
    destination: &impl Fn() -> std::process::Command,
    paths: &plakd::home::paths::PlakPaths,
    sessions: &[String; 2],
) {
    let live = rusqlite::Connection::open(paths.product_database()).unwrap();
    live.execute_batch("BEGIN; SELECT name FROM sqlite_schema;")
        .unwrap();
    let probe = |mut command: std::process::Command, session: &str, host_upgrade| {
        command.arg("version");
        if host_upgrade {
            command.arg("--host-upgrade-reopen");
        }
        let output = command.args(["--reopen-probe", session]).output().unwrap();
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    assert_eq!(
        probe(fixture.command(), &sessions[0], false),
        json!({"verdict":"opens"})
    );
    assert_eq!(
        probe(fixture.command(), &sessions[1], true),
        json!({"verdict":"opens"})
    );
    let refused = probe(destination(), &sessions[0], true);
    assert_eq!(refused["verdict"], "refused");
    assert!(
        refused["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("product store upgrade pending")),
        "{refused}"
    );
}

fn assert_parallel_inspection(
    command: &impl Fn() -> std::process::Command,
    stores: &std::path::Path,
    sessions: &[String; 2],
) {
    let paths = ["product.sqlite3", "subagents.sqlite3"].map(|name| stores.join(name));
    let pinned = paths.each_ref().map(|path| {
        let copy = rusqlite::Connection::open(path).unwrap();
        let mode = |sql| {
            copy.query_row(sql, [], |row| row.get::<_, String>(0))
                .unwrap()
        };
        assert_eq!(mode("PRAGMA journal_mode"), "wal");
        // Give the two readers a database on which any attempt to configure
        // WAL (rather than inspect it) must refuse, independently of timing.
        assert_eq!(mode("PRAGMA journal_mode=DELETE"), "delete");
        copy.execute_batch("BEGIN; SELECT name FROM sqlite_schema;")
            .unwrap();
        copy
    });
    let before = paths.each_ref().map(|path| std::fs::read(path).unwrap());
    let gate = tempfile::tempdir_in(stores.parent().unwrap()).unwrap();
    let probes = sessions.each_ref().map(|session| {
        command()
            .args([
                "version",
                "--host-upgrade-reopen",
                "--reopen-probe",
                session,
            ])
            .arg("--store-probe")
            .arg(stores)
            .env("PLAK_TEST_HOST_REOPEN_GATE", gate.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline
        && !sessions
            .iter()
            .all(|session| gate.path().join(session).is_file())
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let met = sessions
        .iter()
        .all(|session| gate.path().join(session).is_file());
    write_private(&gate.path().join("release"), "");
    for probe in probes {
        let output = probe.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
            json!({"verdict":"opens"})
        );
    }
    assert!(
        met,
        "both probes must reach the rendezvous before either completes"
    );
    assert_eq!(
        paths.each_ref().map(|path| std::fs::read(path).unwrap()),
        before
    );
    drop(pinned);
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
