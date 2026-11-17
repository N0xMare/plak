//! Same-key probes on paired placements have no native process authority.

use super::*;

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
    let deadline = Instant::now() + WAIT;
    while runner::alive(runner::job(&fixture).0) {
        assert!(Instant::now() < deadline, "source job did not terminate");
        std::thread::sleep(Duration::from_millis(10));
    }
    let state = fixture
        .home()
        .join("state")
        .join(plakd::home::state::state_key());
    let before = dormant::identity(&fixture, &plakd::home::state::state_key(), &session);
    let snapshot = plakous::sessions::SessionBackend::inspect_move_source_unsettled_sqlite(
        state.join("runtime.sqlite3"),
        &[before.0.session_id()],
    )
    .unwrap();
    let branch = snapshot.sessions[0]
        .branches
        .iter()
        .find(|branch| branch.session.branch_id() == before.0.branch_id())
        .unwrap();
    let checkpoint = snapshot.sessions[0]
        .checkpoints
        .iter()
        .find(|checkpoint| checkpoint.id() == branch.checkpoint)
        .unwrap();
    let jobs = checkpoint.manifest().jobs().rows();
    assert!(
        checkpoint.manifest().jobs().next_id() > 1,
        "source must have promoted a job"
    );
    assert!(
        jobs.iter().all(|job| job.terminal.is_some()),
        "source jobs must be terminal"
    );
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
    assert!(
        completed,
        "canonical history must retain the completed process result"
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
    let after = dormant::identity(&fixture, &plakd::home::state::state_key(), &session);
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
                "session_id":"$session_id", "input":"done\n", "disposition":"close", "yield_time_ms":1000
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
