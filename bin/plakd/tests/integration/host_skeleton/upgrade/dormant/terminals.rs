//! An exited original worker is evidence, not an already-consumed agent notice.
use super::*;

#[test]
fn host_move_dormant_retains_an_unconsumed_terminal_job_and_its_output() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let configuration = fixture.home().join("config.toml");
    let text = std::fs::read_to_string(&configuration).unwrap();
    std::fs::write(
        configuration,
        text.replace("enabled = true", "enabled = false"),
    )
    .unwrap();
    let destination = binary(&fixture);
    let release = fixture.root.join("job-release");
    let done = fixture.root.join("job-done");
    let child = fixture.root.join("child.json");
    std::fs::write(&child, "{\"version\":1,\"steps\":[]}").unwrap();
    fixture.write_script(&json!({"version":1,"tree":true,"durable_tree":true,
        "tree_process_tools":true,"product_child_script":child,"steps":[
        {"output":{"tool":{"name":"process_session_start","arguments":{
            "command":format!("n=0; while ! test -f '{}'; do n=$((n + 1)); [ \"$n\" -le 1200 ] || exit 71; sleep 0.05; done; printf retained-terminal-output; printf done > '{}'", release.display(), done.display()),
            "workdir":".","login":false,"tty":false,"keep_stdin_open":false,
            "yield_time_ms":0,"max_output_bytes":4096
        }}},"finish":"tool_calls"},{"output":{"text":ANSWER}}
    ]}));
    let mut host = start_env(&fixture, &[], &[(ENABLED, "1".into())]);
    runner::wait_local_runner_ready(&host);
    let (session, node) = finished::create_and_prompt(&fixture, &mut host, "start a retained job");
    finished::wait_idle(&fixture, &session, 1);
    suppress_wake(&fixture, &mut host, &session, &node);
    std::fs::write(&release, "finish").unwrap();
    let deadline = Instant::now() + WAIT;
    while !done.exists() {
        assert!(
            Instant::now() < deadline,
            "retained process did not terminate"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_unobserved_job(&fixture, &session);
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &destination],
        true,
        &[(ENABLED, "1")],
    );
    assert!(result.status.success(), "terminal job refused: {result:?}");
    assert_eq!(successor(&fixture, &session), session);
    let authority = receipts(&fixture).pop().unwrap();
    let group = authority["groups"][0]["id"].as_str().unwrap();
    let capsule: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            fixture
                .home()
                .join("host/upgrades")
                .join(authority["id"].as_str().unwrap())
                .join(format!("{group}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    let jobs = &capsule["sessions"][0]["heads"][0]["jobs"];
    assert_eq!(jobs["next_number"], 2);
    assert_eq!(
        jobs["rows"].as_array().unwrap().len(),
        1,
        "terminal proof must not consume the native notice"
    );
    assert_eq!(
        jobs["rows"][0]["terminal"]["tail"],
        "retained-terminal-output"
    );
    stop(&mut host);
}

fn assert_unobserved_job(fixture: &Fixture, session: &str) {
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let locator = plakd::store::sessions::load_session(&paths, session)
        .unwrap()
        .plakous_locator
        .unwrap();
    let source = plakous::sessions::SessionBackend::inspect_move_source_unsettled_sqlite(
        paths.runtime_database(),
        &[locator.session_id()],
    )
    .unwrap();
    let jobs = source
        .sessions
        .iter()
        .flat_map(|session| &session.checkpoints)
        .flat_map(|checkpoint| checkpoint.manifest().jobs().rows())
        .collect::<Vec<_>>();
    assert_eq!(jobs.len(), 1);
    assert!(
        jobs[0].terminal.is_none(),
        "fixture must reach the original unobserved-terminal refusal"
    );
    assert!(source.lines.iter().all(|line| line.terminals.is_empty()));
    assert!(
        source.settled().is_err(),
        "a completed process alone is not durable native terminal evidence"
    );
}

fn suppress_wake(fixture: &Fixture, host: &mut Host, session: &str, node: &NodeAddress) {
    // Ordinary idle Esc holds automatic notices, without stopping the promoted process.
    let mut client = Client::connect(host);
    let attachment = client.attach(1, node);
    assert_eq!(
        client.request(
            2,
            Op::Intent {
                attachment,
                intent: SessionIntent::Interrupt,
                client_id: None,
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let deadline = Instant::now() + WAIT;
    while !plakd::store::sessions::load_session(&paths, session)
        .unwrap()
        .child_wake_suppressed
    {
        assert!(
            Instant::now() < deadline,
            "idle interrupt did not hold automatic notices"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        client.request(3, Op::Detach { attachment }),
        Outcome::Ok(Answer::Detached)
    );
}

#[test]
fn host_move_dormant_preparation_records_an_original_terminal_after_source_sigkill() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut host, session) = dormant_original(&fixture, None);
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &binary(&fixture)],
        true,
        &[(ENABLED, "1")],
    );
    assert!(result.status.success(), "prepare proof refused: {result:?}");
    assert_eq!(successor(&fixture, &session), session);
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let db = rusqlite::Connection::open_with_flags(
        paths.runtime_database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let bytes: Vec<u8> = db
        .query_row(
            "SELECT evidence FROM line_job_terminals WHERE job=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let terminal = plakous::sessions::host_move::MoveLineTerminal::decode(&bytes).unwrap();
    assert_eq!(terminal.terminal.tail, "prepare-original-tail");
    let facts = db
        .prepare("SELECT record FROM session_feed")
        .unwrap()
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|row| plakous::sessions::feed::FeedRecord::decode(&row.unwrap()).unwrap())
        .flat_map(|record| record.facts().to_vec())
        .filter(|fact| {
            matches!(
                fact,
                plakous::sessions::feed::FeedFact::LineJobTerminalRecorded { job: 1, .. }
            )
        })
        .count();
    assert_eq!(
        facts, 1,
        "the move hold must record the original terminal once"
    );
    stop(&mut host);
}

#[test]
fn host_move_dormant_preparation_rechecks_original_proofs_before_prepare_and_surrender() {
    for fault in [
        "terminal-proof-before-prepare",
        "terminal-proof-before-surrender",
    ] {
        assert_prepare_refusal(
            fault,
            "sampled original runner terminal changed or disappeared",
        );
    }
}

#[test]
fn host_move_dormant_preparation_refuses_a_matching_number_with_foreign_allocation_or_generation() {
    for fault in [
        "terminal-proof-wrong-session",
        "terminal-proof-wrong-generation",
    ] {
        assert_prepare_refusal(
            fault,
            "unconsumed job lacks an exact original-worker terminal",
        );
    }
}

fn assert_prepare_refusal(fault: &str, expected: &str) {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (mut host, _session) = dormant_original(&fixture, Some(fault));
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &binary(&fixture)],
        true,
        &[(ENABLED, "1")],
    );
    assert!(
        !result.status.success(),
        "fault {fault} surrendered the source: {result:?}"
    );
    assert!(
        String::from_utf8_lossy(&result.stderr).contains(expected),
        "{fault}: {result:?}"
    );
    if fixture.home().join("host/upgrades").exists() {
        assert!(
            receipts(&fixture)
                .iter()
                .all(|receipt| receipt["phase"] == "aborted")
        );
    }
    let mut client = Client::connect(&host);
    assert!(
        matches!(
            client.request(1, new_session(&fixture.workspace())),
            Outcome::Ok(Answer::Created { .. })
        ),
        "source admissions stayed paused after {fault}"
    );
    stop(&mut host);
}

fn dormant_original(fixture: &Fixture, fault: Option<&str>) -> (Host, String) {
    let configuration = fixture.home().join("config.toml");
    let text = std::fs::read_to_string(&configuration).unwrap();
    std::fs::write(
        configuration,
        text.replace("enabled = true", "enabled = false"),
    )
    .unwrap();
    let release = fixture.root.join("prepare-release");
    let child = fixture.root.join("child.json");
    std::fs::write(&child, "{\"version\":1,\"steps\":[]}").unwrap();
    fixture.write_script(&json!({"version":1,"tree":true,"durable_tree":true,
        "tree_process_tools":true,"product_child_script":child,"steps":[
        {"output":{"tool":{"name":"process_session_start","arguments":{
            "command":format!("n=0; while ! test -f '{}'; do n=$((n + 1)); [ \"$n\" -le 1200 ] || exit 71; sleep 0.05; done; printf prepare-original-tail", release.display()),
            "workdir":".","login":false,"tty":false,"keep_stdin_open":false,
            "yield_time_ms":0,"max_output_bytes":4096
        }}},"finish":"tool_calls"},{"output":{"text":ANSWER}}
    ]}));
    let mut host = start_env(fixture, &[], &[(ENABLED, "1".into())]);
    runner::wait_local_runner_ready(&host);
    let (session, node) = finished::create_and_prompt(fixture, &mut host, "start the original job");
    finished::wait_idle(fixture, &session, 1);
    suppress_wake(fixture, &mut host, &session, &node);
    assert!(runner::local_daemon_lines(fixture).iter().any(|line| {
        line.jobs["jobs"]
            .as_array()
            .is_some_and(|jobs| jobs.iter().any(|job| job["status"]["status"] == "running"))
    }));
    kill(&mut host);
    let mut env = vec![(ENABLED, "1".into())];
    if let Some(fault) = fault {
        env.push(("PLAK_TEST_HOST_UPGRADE_FAILURE", fault.into()));
    }
    let host = start_env(fixture, &[], &env);
    runner::wait_local_runner_ready(&host);
    std::fs::write(release, "finish").unwrap();
    let deadline = Instant::now() + WAIT;
    while !runner::local_daemon_lines(fixture).iter().any(|line| {
        line.jobs["jobs"]
            .as_array()
            .is_some_and(|jobs| jobs.len() == 1 && jobs[0]["status"]["status"] == "exited")
    }) {
        assert!(Instant::now() < deadline, "original worker did not finish");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut client = Client::connect(&host);
    assert_eq!(
        client
            .rows(1)
            .iter()
            .find(|row| row.session == session)
            .unwrap()
            .residency,
        SessionResidency::Dormant
    );
    assert_unobserved_job(fixture, &session);
    (host, session)
}
