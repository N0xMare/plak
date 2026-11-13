//! Same-key handoff keeps identities and interrupts only the old admitted call.

use super::*;
use plak::store::session_records::{SessionRecordStore as _, SqliteSessionStore};
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};

mod observations;
mod paid_fold;
mod process;
mod protocol;
mod stop;

fn store(fixture: &Fixture) -> SqliteSessionStore {
    SqliteSessionStore::new(plak::home::paths::PlakPaths::explicit(fixture.home()))
}

fn dispatches(fixture: &Fixture) -> usize {
    read_evidence(fixture.root.join("evidence.jsonl"))
        .unwrap()
        .iter()
        .filter(|row| row.kind == EvidenceKind::ProviderDispatched)
        .count()
}

fn settled(fixture: &Fixture, session: &str) {
    let path = fixture.root.join(format!("settled-{session}.json"));
    let deadline = Instant::now() + WAIT;
    loop {
        let result = output(
            fixture,
            &[
                "session",
                "export",
                session,
                "--out",
                path.to_str().unwrap(),
            ],
            false,
        );
        if result.status.success() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the continued session did not settle: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn busy(fixture: &Fixture) -> (Host, Client, String, NodeAddress, PathBuf) {
    let release = fixture.root.join("provider-release");
    let host = start_env(
        fixture,
        &[],
        &[("PLAK_TEST_PROVIDER_RELEASE", release.display().to_string())],
    );
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created { session, node, .. }) =
        client.request(1, new_session(&fixture.workspace()))
    else {
        panic!("not created")
    };
    let attachment = client.attach(2, &node);
    assert_eq!(
        client.request(
            3,
            Op::Intent {
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                attachment,
                intent: SessionIntent::Prompt(Draft::from("preserve this admitted input")),
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    wait_for_provider_dispatch(fixture);
    (host, client, session, node, release)
}

fn fresh_session(fixture: &Fixture) -> Op<PlakExtension> {
    let mut op = new_session(&fixture.workspace());
    let Op::NewSession { request_id, .. } = &mut op else {
        unreachable!()
    };
    *request_id = uuid::Uuid::new_v4().to_string();
    op
}

fn prompt(client: &mut Client, attachment: u64, text: &str) {
    assert_eq!(
        client.request(
            20,
            Op::Intent {
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                attachment,
                intent: SessionIntent::Prompt(Draft::from(text)),
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
}

fn process_script(fixture: &Fixture) -> serde_json::Value {
    let command = format!(
        "touch '{}'; n=0; while [ ! -f '{}' ]; do [ \"$n\" -lt 30000 ] || exit 1; n=$((n + 1)); sleep 0.02; done; printf finished",
        fixture.root.join("process-started").display(),
        fixture.root.join("process-release").display(),
    );
    json!({"version":1,"process_tools":true,"steps":[
        {"output":{"tool":{"name":"process_session_start","arguments":{
            "command":command,"workdir":".","login":false,"tty":false,
            "keep_stdin_open":false,"yield_time_ms":300_000,"max_output_bytes":8192
        }}},"finish":"tool_calls"}, {"output":{"text":ANSWER},"finish":"stop"}
    ]})
}

fn folding_seed(fixture: &Fixture) -> String {
    folding_seed_with(
        fixture,
        &json!({"version":1,"process_tools":true,"steps":[{"output":{"text":"retained answer"},"finish":"stop"}]}),
    )
}

fn folding_seed_with(fixture: &Fixture, script: &serde_json::Value) -> String {
    fixture.write_script(script);
    let consent = fixture.root.join("seed-consent");
    write_private(&consent, "y\n");
    let first = fixture
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
            &format!("older intent {}", "x".repeat(5000)),
        ])
        .stdin(std::fs::File::open(consent).unwrap())
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let session = String::from_utf8(first.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("session_id="))
        .unwrap()
        .to_owned();
    let second = output(
        fixture,
        &["run", "--session", &session, "--", "new protected intent"],
        false,
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    session
}

#[test]
fn host_step_drain_tool_and_paid_fold_at_bound_refuse_without_cancelling_either() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let provider_release = fixture.root.join("provider-release");
    std::fs::write(&provider_release, "continue").unwrap();
    let process_started = fixture.root.join("process-started");
    let process_release = fixture.root.join("process-release");
    let folding_session = folding_seed(&fixture);
    fixture.write_script(&process_script(&fixture));
    let mut host = start_env(
        &fixture,
        &[],
        &[(
            "PLAK_TEST_PROVIDER_RELEASE",
            provider_release.display().to_string(),
        )],
    );
    let mut client = Client::connect(&host);
    let Outcome::Ok(Answer::Created {
        session: process_session,
        node,
        ..
    }) = client.request(1, fresh_session(&fixture))
    else {
        panic!("process root")
    };
    let attachment = client.attach(2, &node);
    prompt(&mut client, attachment, "run the joined foreground process");
    let deadline = Instant::now() + WAIT;
    while !process_started.exists() {
        assert!(Instant::now() < deadline, "process never dispatched");
        std::thread::sleep(Duration::from_millis(10));
    }
    let node = client
        .rows(3)
        .into_iter()
        .find(|row| row.session == folding_session)
        .unwrap()
        .node;
    let folding_attachment = client.attach(4, &node);
    std::fs::remove_file(&provider_release).unwrap();
    let summaries = compact_and_wait(&fixture, &mut client, folding_attachment, deadline);
    let refused = output_env(
        &fixture,
        &[
            "host",
            "upgrade",
            "--expect-binary",
            &binary(&fixture),
            "--drain",
            "0",
        ],
        false,
        &[("PLAK_TEST_FORCE_HOST_UPGRADE", "1")],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("host_step_drain_blocked"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(client.request(6, Op::Ping), Outcome::Ok(Answer::Pong));
    assert!(!fixture.home().join("host/upgrades").exists());
    std::fs::write(&provider_release, "continue").unwrap();
    settled(&fixture, &folding_session);
    assert_eq!(
        read_evidence(summaries).unwrap().len(),
        1,
        "paid fold is never retried"
    );
    finish_process_after_rollback(&fixture, &mut client, &process_session);
    std::fs::write(&process_release, "continue").unwrap();
    stop(&mut host);
}

fn finish_process_after_rollback(fixture: &Fixture, client: &mut Client, process_session: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        let views = store(fixture).load_views(process_session).unwrap().unwrap();
        if views.state.counters.root_turns == 1 && views.state.open_turns.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "joined process result did not finish after rollback"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let process = store(fixture).load_intents(process_session).unwrap();
    assert_eq!(process.root_continuations.len(), 0);
    assert_eq!(
        store(fixture)
            .load(process_session)
            .unwrap()
            .root_turn_count,
        1
    );
    assert_eq!(
        dispatches(fixture),
        5,
        "two seed turns, one process-call generation, its answer and one fold"
    );
    assert!(
        client
            .rows(7)
            .iter()
            .find(|row| row.session == process_session)
            .unwrap()
            .live_work
            .processes
            > 0,
        "Notice must not stop the promoted process"
    );
    assert_eq!(
        client.request(
            8,
            Op::StopSession {
                session: process_session.to_owned()
            }
        ),
        Outcome::Ok(Answer::Stopped {
            runner_pending: false
        })
    );
}

fn compact_and_wait(
    fixture: &Fixture,
    client: &mut Client,
    attachment: u64,
    deadline: Instant,
) -> PathBuf {
    assert_eq!(
        client.request(
            5,
            Op::Intent {
                client_id: None,
                attachment,
                intent: SessionIntent::Compact,
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    let summaries = fixture.root.join("evidence.summary.jsonl");
    while !read_evidence(&summaries).is_ok_and(|rows| !rows.is_empty()) {
        assert!(Instant::now() < deadline, "paid fold never dispatched");
        std::thread::sleep(Duration::from_millis(10));
    }
    summaries
}

#[test]
fn host_step_drain_zero_plans_one_continuation_without_new_session_ids() {
    let Some(fixture) = fixture() else { return };
    let (mut host, _client, session, node, release) = busy(&fixture);
    let _cleanup = HomeHost(fixture.home());
    let before = store(&fixture).load(&session).unwrap();
    let id = binary(&fixture);
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &id, "--drain", "0"],
        false,
        &[("PLAK_TEST_FORCE_HOST_UPGRADE", "1")],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    std::fs::write(release, "continue").unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        if dispatches(&fixture) == 2
            && matches!(
                plakous_hosted::runtime::root_standing(&store(&fixture), &session).unwrap(),
                plakous_hosted::runtime::RootStanding::Clear
            )
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the planned continuation did not finish exactly once"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let after = store(&fixture).load(&session).unwrap();
    assert_eq!(before.node, after.node);
    let original = before.plakous_locator.unwrap();
    let current = after.plakous_locator.unwrap();
    assert_eq!(original.session_id(), current.session_id());
    assert_eq!(original.branch_id(), current.branch_id());
    assert!(!fixture.home().join("host/upgrades").exists());
    let mut reattached = Client::connect_at(&host.socket);
    reattached.attach(20, &node);
    assert_eq!(reattached.request(21, Op::Ping), Outcome::Ok(Answer::Pong));
    settled(&fixture, &session);
    assert_eq!(dispatches(&fixture), 2);
    stop(&mut host);
}

#[test]
fn host_step_drain_zero_freezes_multiple_sessions_under_one_host_operation() {
    let Some(fixture) = fixture() else { return };
    let (mut host, _first_client, first, _first_node, release) = busy(&fixture);
    let _cleanup = HomeHost(fixture.home());
    let mut second_client = Client::connect(&host);
    let mut create = new_session(&fixture.workspace());
    if let Op::NewSession { request_id, .. } = &mut create {
        *request_id = uuid::Uuid::new_v4().to_string();
    }
    let Outcome::Ok(Answer::Created {
        session: second,
        node,
        ..
    }) = second_client.request(10, create)
    else {
        panic!("not created")
    };
    assert_ne!(first, second);
    let attachment = second_client.attach(11, &node);
    second_client.retain_observations();
    let draft_id = uuid::Uuid::new_v4().to_string().parse().unwrap();
    assert_eq!(
        second_client.request(
            12,
            Op::Intent {
                client_id: Some(draft_id),
                attachment,
                intent: SessionIntent::Prompt(Draft::from("another admitted input")),
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
    loop {
        if matches!(second_client.next(), HostFrame::Event {
            client_id: Some(id),
            observation: plakous_host_protocol::SessionObservation::DraftAccepted(_),
            ..
        } if id == draft_id)
        {
            break;
        }
    }
    let deadline = Instant::now() + WAIT;
    while dispatches(&fixture) < 2 {
        assert!(Instant::now() < deadline, "both sessions did not dispatch");
        std::thread::sleep(Duration::from_millis(10));
    }
    let original = [first, second].map(|id| {
        let record = store(&fixture).load(&id).unwrap();
        (id, record.node, record.plakous_locator.unwrap())
    });
    let id = binary(&fixture);
    let result = output_env(
        &fixture,
        &["host", "upgrade", "--expect-binary", &id, "--drain", "0"],
        false,
        &[("PLAK_TEST_FORCE_HOST_UPGRADE", "1")],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    std::fs::write(release, "continue").unwrap();
    let deadline = Instant::now() + WAIT;
    while dispatches(&fixture) != 4 {
        assert!(
            Instant::now() < deadline,
            "one continuation per session did not dispatch"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    for (session, node, locator) in original {
        settled(&fixture, &session);
        let record = store(&fixture).load(&session).unwrap();
        assert_eq!(record.node, node);
        assert_eq!(
            record.plakous_locator.unwrap().session_id(),
            locator.session_id()
        );
        let mut client = Client::connect_at(&host.socket);
        client.attach(
            20,
            &NodeAddress {
                tree_id: node.tree_id,
                agent_id: node.agent_id,
            },
        );
        assert_eq!(client.request(21, Op::Ping), Outcome::Ok(Answer::Pong));
    }
    assert_eq!(dispatches(&fixture), 4);
    stop(&mut host);
}

struct Requester(Option<std::process::Child>);

impl Drop for Requester {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn natural_boundary(mode: &str) {
    let Some(fixture) = fixture() else { return };
    let (mut host, mut client, session, _node, release) = busy(&fixture);
    let _cleanup = HomeHost(fixture.home());
    let id = binary(&fixture);
    let mut command = fixture.command();
    command
        .args(["host", "upgrade", "--expect-binary", &id, "--drain", mode])
        .env("PLAK_TEST_FORCE_HOST_UPGRADE", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut requester = Requester(Some(command.spawn().unwrap()));
    let deadline = Instant::now() + WAIT;
    loop {
        match client.request(10, new_session(&fixture.root.join("absent-workspace"))) {
            Outcome::Refused(reason) => {
                if format!("{reason:?}").contains("upgrad") {
                    break;
                }
            }
            other @ Outcome::Ok(_) => panic!("unexpected admission probe: {other:?}"),
        }
        assert!(
            Instant::now() < deadline,
            "upgrade never stopped admissions"
        );
        assert!(
            requester.0.as_mut().unwrap().try_wait().unwrap().is_none(),
            "upgrade ended before its admission pause"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(requester.0.as_mut().unwrap().try_wait().unwrap().is_none());
    assert_eq!(dispatches(&fixture), 1);
    std::fs::write(release, "continue").unwrap();
    while requester.0.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "natural step drain did not finish"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = requester.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(dispatches(&fixture), 1);
    assert!(matches!(
        plakous_hosted::runtime::root_standing(&store(&fixture), &session).unwrap(),
        plakous_hosted::runtime::RootStanding::Clear
    ));
    stop(&mut host);
}

#[test]
fn host_step_drain_finite_finishes_the_existing_call_without_continuation() {
    natural_boundary("1m");
}

#[test]
fn host_step_drain_idle_finishes_the_existing_call_without_continuation() {
    natural_boundary("idle");
}

#[test]
fn host_step_drain_requester_loss_reopens_admissions_and_finishes_the_same_turn() {
    requester_loss("1m");
}

#[test]
fn host_step_drain_idle_progress_and_ctrl_c_reopen_admissions() {
    requester_loss("idle");
}

fn requester_loss(mode: &str) {
    let Some(fixture) = fixture() else { return };
    let (mut host, mut client, session, _node, release) = busy(&fixture);
    let _cleanup = HomeHost(fixture.home());
    let id = binary(&fixture);
    let progress = fixture.root.join("upgrade-progress.log");
    let mut command = fixture.command();
    command
        .args(["host", "upgrade", "--expect-binary", &id, "--drain", mode])
        .env("PLAK_TEST_FORCE_HOST_UPGRADE", "1")
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&progress).unwrap());
    let mut requester = Requester(Some(command.spawn().unwrap()));
    let deadline = Instant::now() + WAIT;
    loop {
        if let Outcome::Refused(reason) =
            client.request(10, new_session(&fixture.root.join("absent-workspace")))
            && format!("{reason:?}").contains("upgrad")
        {
            break;
        }
        assert!(Instant::now() < deadline, "upgrade never paused admissions");
        assert!(requester.0.as_mut().unwrap().try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
    if mode == "idle" {
        loop {
            let message = std::fs::read_to_string(&progress).unwrap();
            if message.contains("model=1") && message.contains("waiting ") {
                break;
            }
            assert!(Instant::now() < deadline, "no model progress: {message}");
            std::thread::sleep(Duration::from_millis(20));
        }
        let pid = i32::try_from(requester.0.as_ref().unwrap().id()).unwrap();
        assert!(
            std::process::Command::new("kill")
                .args(["-INT", &pid.to_string()])
                .status()
                .unwrap()
                .success()
        );
    } else {
        requester.0.as_mut().unwrap().kill().unwrap();
    }
    requester.0.take().unwrap().wait().unwrap();
    loop {
        match client.request(11, new_session(&fixture.root.join("absent-workspace"))) {
            Outcome::Refused(reason) if !format!("{reason:?}").contains("upgrad") => break,
            Outcome::Refused(_) => {}
            other @ Outcome::Ok(_) => panic!("unexpected probe outcome: {other:?}"),
        }
        assert!(
            Instant::now() < deadline,
            "requester loss did not reopen admissions"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(release, "continue").unwrap();
    settled(&fixture, &session);
    assert_eq!(
        dispatches(&fixture),
        1,
        "requester loss must not cancel or continue the admitted turn"
    );
    assert_eq!(root_record(&fixture, &session).1, 0);
    assert_eq!(client.request(12, Op::Ping), Outcome::Ok(Answer::Pong));
    assert!(!fixture.home().join("host/upgrades").exists());
    stop(&mut host);
}
