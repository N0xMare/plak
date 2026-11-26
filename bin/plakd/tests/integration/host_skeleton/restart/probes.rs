//! The read-only pre-stop probes and the plain operator verbs a restart
//! rollout uses, against a serving home.

use super::*;

fn json_output(fixture: &Fixture, arguments: &[&str]) -> (bool, serde_json::Value) {
    let result = output(fixture, arguments);
    let value = serde_json::from_slice(&result.stdout).unwrap_or_else(|error| {
        panic!(
            "{arguments:?}: {error}; stdout={}; stderr={}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        )
    });
    (result.status.success(), value)
}

fn revision(path: &Path) -> i64 {
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn pre_stop_probes_read_the_live_stores_and_the_wake_verb_reaches_the_host() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    fixture.write_script(&json!({
        "version": 1, "repeat_last_step": true,
        "steps": [{"output": {"text": ANSWER}}]
    }));
    let session = seed(&fixture, 7);
    let mut host = start(&fixture);
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let before = (
        revision(&paths.product_database()),
        revision(&paths.subagents_database()),
    );

    let rehearsal = fixture.home().join(".rehearsal");
    let (ok, report) = json_output(
        &fixture,
        &["store", "rehearse", "--into", rehearsal.to_str().unwrap()],
    );
    assert!(ok, "{report}");
    assert_eq!(report["revisions_pending"], json!(false));
    let stores = report["stores"].as_str().unwrap().to_owned();
    assert!(Path::new(&stores).join("product.sqlite3").is_file());
    assert!(
        !output(
            &fixture,
            &["store", "rehearse", "--into", rehearsal.to_str().unwrap()],
        )
        .status
        .success(),
        "an existing rehearsal directory is refused"
    );

    // The offline fixture's loopback executor is not release-owned, so its
    // rows read unavailable; the verdicts must match on live and rehearsed
    // stores either way.
    let mut verdicts = Vec::new();
    for probe in [None, Some(stores.as_str())] {
        let mut binding = vec!["session", "check-binding", "--gating"];
        let mut reopen = vec!["session", "check-reopen", session.as_str()];
        if let Some(stores) = probe {
            binding.extend(["--store-probe", stores]);
            reopen.extend(["--store-probe", stores]);
        }
        let (_, report) = json_output(&fixture, &binding);
        assert_eq!(report["sessions"].as_array().unwrap().len(), 1, "{report}");
        let (ok, verdict) = json_output(&fixture, &reopen);
        assert!(ok, "{verdict}");
        assert!(verdict["verdict"].is_string(), "{verdict}");
        verdicts.push((report, verdict));
    }
    assert_eq!(
        verdicts[0], verdicts[1],
        "rehearsed copies changed a verdict"
    );
    assert_eq!(
        before,
        (
            revision(&paths.product_database()),
            revision(&paths.subagents_database()),
        ),
        "the probes changed a live store revision"
    );

    let (_, doctor) = json_output(&fixture, &["doctor", "--json"]);
    assert_eq!(doctor["version"], json!(1));
    assert_eq!(doctor["state_key"], json!(plakd::home::state::state_key()));
    assert_ne!(doctor["checks"].as_array().unwrap().len(), 0);
    assert!(doctor["summary"]["failures"].is_u64());

    let woke = output(
        &fixture,
        &[
            "host",
            "wake",
            "--session",
            &session,
            "--text",
            "the rollout finished",
        ],
    );
    assert!(
        woke.status.success(),
        "{}",
        String::from_utf8_lossy(&woke.stderr)
    );
    let records = plakd::store::session_records::SqliteSessionStore::new(paths.clone());
    let deadline = Instant::now() + WAIT;
    while {
        use plakd::store::session_records::SessionRecordStore as _;
        records
            .load_views(&session)
            .unwrap()
            .unwrap()
            .state
            .counters
            .root_turns
            == 0
    } {
        assert!(Instant::now() < deadline, "the woken prompt never ran");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut client = Client::connect(&host);
    profiles::wait_idle(&mut client, &session);
    let unknown = output(
        &fixture,
        &[
            "host",
            "wake",
            "--session",
            &uuid::Uuid::new_v4().to_string(),
            "--text",
            "nobody",
        ],
    );
    assert!(!unknown.status.success());
    drop(client);
    stop(&mut host);
    let refused = output(
        &fixture,
        &["host", "wake", "--session", &session, "--text", "no host"],
    );
    assert!(!refused.status.success(), "wake must not start a host");
}

#[test]
fn pending_revision_is_rehearsed_and_both_probes_read_the_revised_copies() {
    let Some(fixture) = fixture() else { return };
    let session = seed(&fixture, 8);
    let mut host = start(&fixture);
    let image = Path::new(env!("CARGO_BIN_EXE_plakd"))
        .canonicalize()
        .unwrap();
    let candidate = |args: &[&str]| {
        let result = image_command(&fixture, &image)
            .env("PLAK_TEST_STORE_REVISION_IMAGE", &image)
            .args(args)
            .output()
            .unwrap();
        serde_json::from_slice::<serde_json::Value>(&result.stdout)
            .unwrap_or_else(|error| panic!("{args:?}: {error}: {result:?}"))
    };
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let product = revision(&paths.product_database());
    let tree = revision(&paths.subagents_database());
    let into = fixture.home().join(".pending-rehearsal");
    let report = candidate(&["store", "rehearse", "--into", into.to_str().unwrap()]);
    assert_eq!(report["revisions_pending"], json!(true));
    let stores = report["stores"].as_str().unwrap();
    assert_eq!(
        revision(&Path::new(stores).join("product.sqlite3")),
        product + 1
    );
    assert_eq!(
        revision(&Path::new(stores).join("subagents.sqlite3")),
        tree + 1
    );
    let binding = candidate(&[
        "session",
        "check-binding",
        "--gating",
        "--store-probe",
        stores,
    ]);
    assert_eq!(binding["sessions"].as_array().unwrap().len(), 1);
    let live = candidate(&["session", "check-reopen", &session]);
    assert_eq!(live["verdict"], "refused", "{live}");
    assert!(
        live["reason"].as_str().unwrap().contains("pending"),
        "{live}"
    );
    let reopen = candidate(&["session", "check-reopen", &session, "--store-probe", stores]);
    assert!(reopen["verdict"].is_string());
    assert_ne!(
        reopen, live,
        "the reopen probe did not read the revised copies"
    );
    assert_eq!(revision(&paths.product_database()), product);
    assert_eq!(revision(&paths.subagents_database()), tree);
    stop(&mut host);
}
