//! Same-key probes on paired placements have no native process authority.

use super::*;

#[test]
fn a_head_advanced_during_qualification_is_checked_at_handoff() {
    let Some(fixture) = fixture() else { return };
    fixture.write_script(&json!({
        "version": 1,
        "steps": [{"output": {"text": "first"}}, {"output": {"text": "second"}}]
    }));
    let gate = fixture.root.join("reopen-gate");
    std::fs::create_dir(&gate).unwrap();
    let mut host = start_env(
        &fixture,
        &[],
        &[("PLAK_TEST_HOST_REOPEN_GATE", gate.display().to_string())],
    );
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("root creation refused");
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "first turn", "first");
    profiles::wait_idle(&mut client, &session);
    let key = plakd::home::state::state_key();
    let locator = identity(&fixture, &key, &session).0;
    let database = plakd::home::paths::PlakPaths::explicit(fixture.home()).runtime_database();
    let revision = || {
        plakous::sessions::AgentSessions::inspect_branch_revision_sqlite(&database, locator)
            .unwrap()
    };
    let before = revision();
    let request = std::thread::scope(|scope| {
        let request = scope.spawn(|| same_key_upgrade(&fixture, &std::env::var("PATH").unwrap()));
        let deadline = Instant::now() + WAIT;
        while !gate.join(&session).is_file() {
            assert!(
                Instant::now() < deadline,
                "reopen probe did not read the head"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        client.prompt(4, attachment, "advance during qualification", "second");
        profiles::wait_idle(&mut client, &session);
        let deadline = Instant::now() + WAIT;
        while revision() <= before {
            assert!(Instant::now() < deadline, "runtime head did not advance");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(gate.join("release"), "").unwrap();
        request.join().unwrap()
    });
    let stderr = String::from_utf8_lossy(&request.stderr);
    assert!(request.status.success(), "{stderr}");
    assert!(
        gate.join(format!("{session}.rechecked")).is_file(),
        "{stderr}"
    );
    let (qualification, _) = stderr
        .split_once("resolving earlier admission verdicts")
        .unwrap();
    let phases: Vec<_> = qualification
        .lines()
        .filter_map(|line| {
            let timing = line.strip_prefix("host upgrade: qualification phase=")?;
            let (phase, elapsed_ms) = timing.split_once(" elapsed_ms=").unwrap();
            assert!(elapsed_ms.parse::<u64>().is_ok(), "{line}");
            Some(phase)
        })
        .collect();
    assert!(
        matches!(
            phases.as_slice(),
            [
                "image",
                "capability",
                "binding_source",
                "binding_destination",
                "exact_reopen",
                "runner_process",
                "standalone_admission",
            ] | [
                "image",
                "capability",
                "binding_destination",
                "binding_source",
                "exact_reopen",
                "runner_process",
                "standalone_admission",
            ]
        ),
        "{stderr}"
    );
    stop(&mut host);
}

#[test]
fn a_later_root_refusal_keeps_same_key_admissions_open() {
    let Some(fixture) = fixture() else { return };
    let first = seed(&fixture, 1);
    let second = seed(&fixture, 2);
    let refused_root = first.max(second);
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let product = rusqlite::Connection::open(paths.product_database()).unwrap();
    assert_eq!(
        product
            .execute(
                "UPDATE sessions SET account='fixture-removed-account' WHERE id=?1",
                [&refused_root],
            )
            .unwrap(),
        1
    );
    drop(product);

    let mut host = start(&fixture);
    let mut client = Client::connect(&host);
    let result = same_key_upgrade(&fixture, &std::env::var("PATH").unwrap());
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("host_upgrade_reopen_refused") && stderr.contains(&refused_root),
        "{stderr}"
    );
    assert!(
        !stderr.contains("resolving earlier admission verdicts"),
        "{stderr}"
    );
    runner::wait_local_runner_ready(&host);
    assert!(matches!(
        client.request(1, new_session(&fixture.workspace())),
        Outcome::Ok(Answer::Created { .. })
    ));
    stop(&mut host);
}

#[test]
fn paired_terminal_history_reopens_across_a_changed_triple() {
    terminal_history(true);
}

#[test]
fn paired_terminal_history_reopens_with_an_unchanged_triple() {
    terminal_history(false);
}

fn terminal_history(changed: bool) {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (_daemon, path, id) = runner::remote_fixture(&fixture);
    let (mut host, _client, session, node) = placed_job(&fixture, &path, &id, changed, true);
    let state = fixture
        .home()
        .join("state")
        .join(plakd::home::state::state_key());
    let before = identity(&fixture, &plakd::home::state::state_key(), &session);
    wait_for_terminal_history(&state.join("runtime.sqlite3"), before.0);
    assert!(
        !runner::alive(runner::job(&fixture).0),
        "the recorded terminal must have joined the source job"
    );
    let result = same_key_upgrade(&fixture, &path);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let mut destination = Client::connect_at(&host.socket);
    runner::remote_ready(&mut destination, &id);
    let attachment = destination.attach(20, &node);
    assert_eq!(
        destination.request(21, Op::Detach { attachment }),
        Outcome::Ok(Answer::Detached)
    );
    let after = identity(&fixture, &plakd::home::state::state_key(), &session);
    assert_eq!(after.0.session_id(), before.0.session_id());
    assert_eq!(after.0.branch_id(), before.0.branch_id());
    assert_eq!(after.1, before.1);
    assert_eq!(
        drain::dispatches(&fixture),
        3,
        "upgrade must not replay the process call"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("local-job.marker"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    stop(&mut host);
}

fn wait_for_terminal_history(path: &std::path::Path, source: plakous::sessions::SessionLocator) {
    let deadline = Instant::now() + WAIT;
    loop {
        let checkpoint =
            plakous_offline_crash_harness::ledger_signature::inspect_branch_checkpoint(
                path, source,
            )
            .unwrap();
        let jobs = checkpoint.manifest().jobs();
        let promoted = jobs.next_id() > 1;
        let terminal = jobs.rows().iter().all(|job| job.terminal.is_some());
        let completed = checkpoint
            .manifest()
            .canonical()
            .history()
            .segments()
            .iter()
            .flat_map(plakous::model::HistorySegment::items)
            .flat_map(plakous::model::ConversationItem::blocks)
            .filter_map(|block| match block {
                plakous::model::ContentBlock::ToolResult(result) => Some(result.parts()),
                _ => None,
            })
            .flatten()
            .filter_map(|part| match part {
                plakous::model::ToolResultPart::Text(text) => {
                    serde_json::from_str::<serde_json::Value>(text.text()).ok()
                }
                _ => None,
            })
            .any(|result| result["exit_code"] == 0);
        if promoted && terminal && completed {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "source did not record terminal history: promoted={promoted}, terminal={terminal}, completed={completed}",
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn paired_live_job_changed_triple_refuses_with_old_build_remedy() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (_daemon, path, id) = runner::remote_fixture(&fixture);
    let (mut host, mut client, session, _) = placed_job(&fixture, &path, &id, true, false);
    let result = same_key_upgrade(&fixture, &path);
    assert!(!result.status.success());
    let message = String::from_utf8_lossy(&result.stderr);
    assert!(
        message.contains("host_upgrade_process_retention_unavailable"),
        "{message}"
    );
    assert!(
        message.contains("settle and drain them using the old build, then retry"),
        "{message}"
    );
    assert!(!message.contains("Internal"), "{message}");
    assert!(
        runner::alive(runner::job(&fixture).0),
        "refusal must leave source custody intact"
    );
    assert_eq!(client.request(10, Op::Ping), Outcome::Ok(Answer::Pong));
    assert_eq!(
        client.request(11, Op::StopSession { session }),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
    assert!(!fixture.home().join("host/upgrades").exists());
    stop(&mut host);
}

#[test]
fn paired_live_job_unchanged_triple_probe_keeps_source_custody() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let (_daemon, path, id) = runner::remote_fixture(&fixture);
    let (mut host, mut client, session, _) = placed_job(&fixture, &path, &id, false, false);
    let pid = runner::job(&fixture).0;
    assert!(runner::alive(pid));
    let result = output_env(
        &fixture,
        &[
            "version",
            "--host-upgrade-reopen",
            "--reopen-probe",
            &session,
        ],
        false,
        &[("PATH", &path)],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap(),
        json!({"verdict":"opens"})
    );
    assert!(runner::alive(pid));
    assert_eq!(runner::job(&fixture).0, pid);
    assert_eq!(drain::dispatches(&fixture), 2);
    assert_eq!(
        client.request(21, Op::StopSession { session }),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
    stop(&mut host);
}

pub(super) fn placed_job(
    fixture: &Fixture,
    path: &str,
    id: &str,
    changed: bool,
    terminal: bool,
) -> (Host, Client, String, NodeAddress) {
    if terminal {
        let mut script: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
        script["steps"].as_array_mut().unwrap().insert(1, json!({
            "output":{"tool":{"name":"process_session_write","arguments":{
                "session_id":"$session_id", "input":"done\n", "disposition":"close", "yield_time_ms":30_000
            }}},"finish":"tool_calls"
        }));
        fixture.write_script(&script);
    }
    // The destination-probe hook changes the triple comparison, not the real
    // paired connection or its qualification request restrictions.
    let mut environment = vec![("PATH", path.to_owned())];
    if changed {
        environment.push((
            "PLAK_TEST_HOST_UPGRADE_RUNNER_REVISION",
            "destination-revision".into(),
        ));
    }
    let host = start_env(fixture, &[], &environment);
    let mut client = Client::connect(&host);
    runner::remote_ready(&mut client, id);
    let mut request = new_session(&fixture.workspace());
    if let Op::NewSession { runner, .. } = &mut request {
        *runner = plakous_host_protocol::wire::RunnerLabel {
            name: "remote".into(),
            runner_id: id.into(),
        };
    }
    let Outcome::Ok(Answer::Created { session, node, .. }) = client.request(1, request) else {
        panic!("remote creation refused")
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "start a job on the paired placement", ANSWER);
    runner::job(fixture);
    (host, client, session, node)
}

pub(super) fn same_key_upgrade(fixture: &Fixture, path: &str) -> std::process::Output {
    output_env(
        fixture,
        &[
            "host",
            "upgrade",
            "--expect-binary",
            &binary(fixture),
            "--drain",
            "0",
        ],
        false,
        &[("PATH", path), ("PLAK_TEST_FORCE_HOST_UPGRADE", "1")],
    )
}
