//! A finite handoff must not spend model grace on a joined process promotion.

use super::*;
use plakd::store::session_records::{SessionRecordStore as _, SqliteSessionStore};
use plakous_offline_crash_harness::ledger_signature::{inspect_jobs, inspect_ledger};
use plakous_recursive_subagents::tree::{ContinuationCause, ProductAgentTree, TreeStore as _};
use plakous_recursive_subagents_store::SqliteTreeStore;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt as _;

mod submitted;

struct FiniteDrain {
    _cleanup: HomeHost,
    paired: Paired,
    fixture: Fixture,
    image: PathBuf,
    environment: Vec<(&'static str, String)>,
}

impl Drop for FiniteDrain {
    fn drop(&mut self) {
        let _ = self
            .open_release()
            .and_then(|mut fifo| fifo.write_all(b"finish\n"));
    }
}

impl FiniteDrain {
    fn new(fixture: Fixture) -> Self {
        let cleanup = HomeHost(fixture.home());
        let image = replacement_image(&fixture).canonicalize().unwrap();
        let paired = Paired::new(&fixture);
        let mut environment = candidate_environment(&image).to_vec();
        environment.push(("PATH", paired.path.clone()));
        Self {
            fixture,
            _cleanup: cleanup,
            image,
            paired,
            environment,
        }
    }

    fn start(&self) -> (Host, Client) {
        self.start_with(None)
    }

    fn start_with(&self, setting: Option<(&str, String)>) -> (Host, Client) {
        let mut environment = self.environment.clone();
        environment.extend(setting);
        let host = start_env(&self.fixture, &[], &environment);
        let mut client = Client::connect(&host);
        runner::remote_ready(&mut client, &self.paired.id);
        (host, client)
    }

    fn upgrade(&self, host: &mut Host, drain: &str) {
        let path = self.fixture.root.join("bounded-upgrade.log");
        let output = std::fs::File::create(&path).unwrap();
        let mut upgrade = self.request_upgrade(drain, output);
        // The gated process cannot exit. Offline probes and exec have ample
        // time, but less than the model grace in the root test.
        let deadline = Instant::now() + WAIT * 2;
        loop {
            if let Some(status) = upgrade.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "{}",
                    std::fs::read_to_string(&path).unwrap()
                );
                assert!(host.child.as_mut().unwrap().try_wait().unwrap().is_none());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{}",
                std::fs::read_to_string(&path).unwrap()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn request_upgrade(&self, drain: &str, output: std::fs::File) -> Child {
        self.paired.advance(&self.image);
        Child::spawn(
            candidate_upgrade_with_drain(&self.fixture, &self.image, &self.environment, drain)
                .stdout(output.try_clone().unwrap())
                .stderr(output),
        )
    }

    fn foreground_plan(&self, child: bool) -> serde_json::Value {
        let root = shell_words::quote(self.fixture.root.to_str().unwrap());
        let command = format!(
            "base={root}/blocked-fg; mkfifo \"$base.release\"; exec 3<>\"$base.release\";\
             printf '%s %s\\n' \"$$\" \"$PPID\" >> \"$base.marker\"; read gate <&3; exit 9"
        );
        let tool = if child {
            json!({"name": "bash", "arguments": {"command": command, "timeout": 600}})
        } else {
            json!({"name": "process_session_start", "arguments": {
                "command": command, "workdir": ".", "login": false, "tty": false,
                "keep_stdin_open": false, "yield_time_ms": 600_000, "max_output_bytes": 4096
            }})
        };
        json!({"version": 1, "process_tools": true, "steps": [{
            "output": {"tool": tool}, "finish": "tool_calls"
        }]})
    }

    fn start_node(&self, client: &mut Client) -> (String, NodeAddress, u64) {
        let mut request = new_session(&self.fixture.workspace());
        if let Op::NewSession { runner, .. } = &mut request {
            *runner = plakous_host_protocol::wire::RunnerLabel {
                name: "remote".into(),
                runner_id: self.paired.id.clone(),
            };
        }
        let created = client.request(1, request);
        let Outcome::Ok(Answer::Created { session, node, .. }) = created else {
            panic!("paired creation refused: {created:?}");
        };
        let attachment = client.attach(2, &node);
        (session, node, attachment)
    }

    fn wait_foreground(&self) -> (i32, i32) {
        let found = marked(&self.fixture, "blocked", "fg");
        assert_eq!(Some(found.1), self.paired.runner.pid());
        found
    }

    fn finish_job(&self, original: (i32, i32)) {
        assert_eq!(original, self.wait_foreground());
        assert!(runner::alive(original.0), "the original job was stopped");
        self.open_release().unwrap().write_all(b"finish\n").unwrap();
        wait_exit(original.0);
    }

    fn open_release(&self) -> std::io::Result<std::fs::File> {
        OpenOptions::new()
            .write(true)
            .custom_flags(rustix::fs::OFlags::NONBLOCK.bits().cast_signed())
            .open(self.fixture.root.join("blocked-fg.release"))
    }
}

#[test]
fn finite_exec_drain_promotes_a_running_root_without_waiting_for_its_job() {
    let Some(fixture) = fixture() else { return };
    let sut = FiniteDrain::new(fixture);
    sut.fixture.write_script(&sut.foreground_plan(false));
    let (mut host, mut client) = sut.start();
    let (session, _, attachment) = sut.start_node(&mut client);
    submit(&mut client, 3, attachment);
    let original = sut.wait_foreground();
    drop(client);

    sut.fixture.write_script(&json!({
        "version": 1, "process_tools": true, "repeat_last_step": true, "steps": [{
            "gate": {"file": sut.fixture.root.join("resume-release")}, "output": {"text": ANSWER}
        }]
    }));
    sut.upgrade(&mut host, "5m");
    let (head, _) = identity(&sut.fixture, &plakd::home::state::state_key(), &session);
    let paths = plakd::home::paths::PlakPaths::explicit(sut.fixture.home());
    sut.finish_job(original);
    wait_retained_terminal(&paths.runtime_database(), head);
    release_continuation(&sut.fixture);
    let mut client = Client::connect(&host);
    profiles::wait_idle(&mut client, &session);
    assert_eq!(root_record(&sut.fixture, &session).1, 1);
    drop(client);
    stop(&mut host);

    assert_eq!(
        inspect_process_notices(&paths.runtime_database(), head).unwrap(),
        [1]
    );
    assert_eq!(marked(&sut.fixture, "blocked", "fg"), original);
}

#[test]
fn finite_exec_drain_promotes_a_running_child_and_continues_it_once() {
    let Some(fixture) = fixture() else { return };
    let sut = FiniteDrain::new(fixture);
    let (mut host, child) = child_with_pending_root(&sut, None);
    sut.upgrade(&mut host, "0");
    finish_child(&sut, &mut host, &child);
}

struct PendingChild {
    session: String,
    parent: NodeAddress,
    original: (i32, i32),
    root_turns: u64,
}

fn child_with_pending_root(
    sut: &FiniteDrain,
    setting: Option<(&str, String)>,
) -> (Host, PendingChild) {
    crash::client_control::script(&sut.fixture, true);
    let child_script = sut.fixture.root.join("child.json");
    std::fs::write(&child_script, sut.foreground_plan(true).to_string()).unwrap();
    let mut parent_script: serde_json::Value =
        serde_json::from_slice(&std::fs::read(sut.fixture.script()).unwrap()).unwrap();
    parent_script["stall_ignores_cancellation"] = json!(true);
    parent_script["steps"][1]["stream"] = json!([
        {"text": PREFIX}, {"gate": {"file": sut.fixture.root.join("parent-never-finishes")}}
    ]);
    sut.fixture.write_script(&parent_script);
    let (host, mut client) = sut.start_with(setting);
    let (session, parent, attachment) = sut.start_node(&mut client);
    client.prompt(3, attachment, "delegate a process", PREFIX);
    let original = sut.wait_foreground();
    let paths = plakd::home::paths::PlakPaths::explicit(sut.fixture.home());
    let root_turns = SqliteSessionStore::new(paths)
        .load_views(&session)
        .unwrap()
        .unwrap()
        .state
        .counters
        .root_turns;
    assert_eq!(root_turns, 1);
    drop(client);

    std::fs::write(
        &child_script,
        json!({
            "version": 1, "process_tools": true,
        "steps": [
            {"gate": {"file": sut.fixture.root.join("resume-release")},
             "output": {"tool": {"name": "wait", "arguments": {"timeout_ms": 30_000}}},
             "finish": "tool_calls"},
            {
                "output": {"tool": {"name": "submit_result", "arguments": {
                "turn_token": 1, "output_json": "{\"summary\":\"review complete\"}"
                }}}, "finish": "tool_calls"
            }]
        })
        .to_string(),
    )
    .unwrap();
    parent_script["steps"] = json!([]);
    parent_script["repeat_last_step"] = json!(false);
    sut.fixture.write_script(&parent_script);
    (
        host,
        PendingChild {
            session,
            parent,
            original,
            root_turns,
        },
    )
}

fn finish_child(sut: &FiniteDrain, host: &mut Host, child: &PendingChild) {
    let deadline = Instant::now() + WAIT;
    while crash::client_control::child_dispatches(&sut.fixture) < 2 {
        assert!(
            Instant::now() < deadline,
            "the child did not resume without an attachment: {:?}",
            host.lines.try_iter().collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let paths = plakd::home::paths::PlakPaths::explicit(sut.fixture.home());
    let store = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
    let before = child_locator(&store, child);
    release_child_wait(
        &sut.fixture,
        &paths.runtime_database(),
        before,
        child.original.0,
    );
    sut.finish_job(child.original);
    root_wake::submitted_result(&sut.fixture, &child.parent);
    root_wake::idle_stays(&sut.fixture, &child.session, child.root_turns);
    assert_eq!(
        root_record(&sut.fixture, &child.session),
        (vec![RootInterruptCause::OwnerLost], 0),
    );
    stop(host);

    let tree = ProductAgentTree::reopen(store.load(&child.parent.tree_id).unwrap().unwrap());
    assert_eq!(loss_resumptions(&tree), 1);
    let locator = child_locator(&store, child);
    assert_eq!(locator.session_id(), before.session_id());
    assert_eq!(locator.branch_id(), before.branch_id());
    assert_one_child_terminal(&paths.runtime_database(), locator);
    let (_, turns) = plakous_offline_crash_harness::ledger_signature::inspect_root_lifecycles(
        &paths.runtime_database(),
        locator,
    )
    .unwrap();
    let distinct_turns = turns
        .iter()
        .map(|turn| turn["turn"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(distinct_turns.len(), 2, "{turns:?}");
    assert!(
        turns
            .iter()
            .all(|turn| matches!(turn["state"].as_str(), Some("completed" | "finalized"))),
        "{turns:?}"
    );
    assert_eq!(marked(&sut.fixture, "blocked", "fg"), child.original);
}

fn child_locator(
    store: &SqliteTreeStore,
    child: &PendingChild,
) -> plakous::sessions::SessionLocator {
    let tree = ProductAgentTree::reopen(store.load(&child.parent.tree_id).unwrap().unwrap());
    tree.nodes()
        .values()
        .find(|node| node.parent_agent_id.is_some())
        .unwrap()
        .session_locator
        .as_deref()
        .unwrap()
        .parse()
        .unwrap()
}

fn wait_retained_terminal(database: &Path, locator: plakous::sessions::SessionLocator) {
    let deadline = Instant::now() + WAIT;
    loop {
        if inspect_jobs(database, locator)
            .unwrap()
            .iter()
            .any(|job| job.number == 1 && job.terminal)
            || inspect_process_notices(database, locator).unwrap() == [1]
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the original runner's terminal did not reach the resumed branch"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn release_child_wait(
    fixture: &Fixture,
    database: &Path,
    locator: plakous::sessions::SessionLocator,
    child_pid: i32,
) {
    let operations = || inspect_ledger(database, locator, "child wait").unwrap();
    let before = operations().iter().map(|op| op.operation).max().unwrap();
    release_continuation(fixture);
    let deadline = Instant::now() + WAIT;
    loop {
        assert!(
            runner::alive(child_pid),
            "the original child job stopped before the test released it"
        );
        // Keep the shell alive until the resumed script dispatches Wait.
        // Dispatch precedes plan resolution; it does not claim the terminal.
        if let Some(wait) = operations()
            .into_iter()
            .filter(|op| op.kind == "effect" && op.operation > before)
            .min_by_key(|op| op.operation)
        {
            if wait.state == "dispatched" {
                return;
            }
            assert!(
                matches!(wait.state, "prepared" | "claimed"),
                "child Wait settled before the original job was released: {wait:?}"
            );
        }
        assert!(Instant::now() < deadline, "the child did not dispatch Wait");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_one_child_terminal(database: &Path, locator: plakous::sessions::SessionLocator) {
    use plakous::model::{ContentBlock, ConversationItem, HistorySegment, ToolResultPart};
    let history =
        plakous::sessions::AgentSessions::export_history_sqlite(database, locator).unwrap();
    let notices = inspect_process_notices(database, locator).unwrap();
    let mut delivered = 0;
    for block in history
        .segments()
        .iter()
        .flat_map(HistorySegment::items)
        .flat_map(ConversationItem::blocks)
    {
        match block {
            ContentBlock::Text(text) if text.text().contains("<process_notice id=\"p1\"") => {
                assert!(text.text().contains("status=\"exited\" exit_code=\"9\""));
                delivered += 1;
            }
            ContentBlock::ToolResult(result) => {
                for part in result.parts() {
                    let ToolResultPart::Text(text) = part else {
                        continue;
                    };
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.text()) else {
                        continue;
                    };
                    for task in value["tasks"].as_array().into_iter().flatten() {
                        if task["task_id"] == "p1" && task["status"] == "exited" {
                            assert_eq!(task["exit_code"], 9);
                            delivered += 1;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    assert!(notices.is_empty() || notices == [1], "{notices:?}");
    assert_eq!(
        delivered, 1,
        "one terminal through the wait result or notice, never both"
    );
}

mod projection;

fn loss_resumptions(tree: &ProductAgentTree) -> usize {
    tree.commands()
        .keys()
        .filter(|id| id.starts_with(ProductAgentTree::LOSS_RESUME_COMMAND_PREFIX))
        .count()
}

#[test]
fn a_child_without_a_handoff_record_waits_for_one_explicit_resume() {
    let Some(fixture) = fixture() else { return };
    let sut = FiniteDrain::new(fixture);
    let (mut host, child) = child_with_pending_root(
        &sut,
        Some((
            "PLAK_TEST_HOST_UPGRADE_PREPARE_FAILURE",
            "child-handoff-no-budget".into(),
        )),
    );
    sut.upgrade(&mut host, "0");
    let mut client = Client::connect(&host);
    runner::remote_ready(&mut client, &sut.paired.id);
    client.retain_observations();
    let attachment = client.attach(10, &child.parent);
    wait_for_notice(&mut client, |text| {
        text.contains("c1") && text.contains("nothing continues until you resume")
    });
    let paths = plakd::home::paths::PlakPaths::explicit(sut.fixture.home());
    let store = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
    let deadline = Instant::now() + WAIT;
    let id = loop {
        let tree = ProductAgentTree::reopen(store.load(&child.parent.tree_id).unwrap().unwrap());
        let id = tree
            .nodes()
            .values()
            .find(|node| node.parent_agent_id.is_some())
            .unwrap()
            .agent_id
            .clone();
        assert_eq!(loss_resumptions(&tree), 0);
        assert_eq!(crash::client_control::child_dispatches(&sut.fixture), 1);
        if tree
            .loss_continuation(&id)
            .is_some_and(|held| held.cause == ContinuationCause::OwnerLost)
        {
            break id;
        }
        assert!(Instant::now() < deadline, "unrecorded child was not held");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        client.request(
            11,
            Op::Intent {
                attachment,
                client_id: None,
                intent: SessionIntent::ResumeLossContinuation {
                    agent_id: Some(id.clone())
                },
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false }),
    );
    let answer = wait_for_notice(&mut client, |text| {
        text.contains(&id)
            && (text.starts_with("resumed ")
                || text.contains("is busy")
                || text.contains("did not start")
                || text.contains("is not held"))
    });
    assert_eq!(answer, format!("resumed #{id}"));
    drop(client);
    finish_child(&sut, &mut host, &child);
    let events = paths
        .session_home()
        .session_events_file(&child.parent.tree_id);
    assert!(
        std::fs::read_to_string(events)
            .unwrap()
            .lines()
            .any(|line| {
                let row: serde_json::Value = serde_json::from_str(line).unwrap();
                row["kind"] == "session.handoff_unrecorded" && row["tree"] == child.parent.tree_id
            })
    );
}

fn wait_for_notice(client: &mut Client, matches: impl Fn(&str) -> bool) -> String {
    use plakous_host_protocol::{SessionObservation, views::transcript::TranscriptRow};
    client.read_deadline = Some(Instant::now() + WAIT);
    loop {
        let HostFrame::Event { observation, .. } = client.next() else {
            continue;
        };
        let rows = match observation {
            SessionObservation::Row(row) => vec![row],
            SessionObservation::TranscriptPatch(patch) => {
                patch.upserts.into_iter().map(|item| item.row).collect()
            }
            SessionObservation::PresentationSnapshot(snapshot) => {
                snapshot.items.into_iter().map(|item| item.row).collect()
            }
            _ => Vec::new(),
        };
        for row in rows {
            if let TranscriptRow::Notice(notice) = row
                && matches(&notice.text)
            {
                client.read_deadline = None;
                return notice.text;
            }
        }
    }
}

pub(super) fn crash_after_child_records() {
    use plakous_hosted::test_support_runner::HostCut;
    use std::os::unix::process::ExitStatusExt as _;
    let Some(fixture) = fixture() else { return };
    let sut = FiniteDrain::new(fixture);
    let (mut host, child) = child_with_pending_root(
        &sut,
        Some((
            "PLAK_TEST_HOST_READOPT_CUT",
            HostCut::AfterChildHandoffs.name().into(),
        )),
    );
    let output = std::fs::File::create(sut.fixture.root.join("child-record-cut.log")).unwrap();
    let mut request = sut.request_upgrade("0", output);
    assert_eq!(
        runner::cut_exit(host.child.as_mut().unwrap()).signal(),
        Some(9)
    );
    let _ = request.kill();
    let _ = request.wait();
    assert_eq!(root_record(&sut.fixture, &child.session), (vec![], 0));
    let paths = plakd::home::paths::PlakPaths::explicit(sut.fixture.home());
    let store = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
    let tree = ProductAgentTree::reopen(store.load(&child.parent.tree_id).unwrap().unwrap());
    let id = tree
        .nodes()
        .values()
        .find(|node| node.parent_agent_id.is_some())
        .unwrap()
        .agent_id
        .as_str();
    assert_eq!(
        tree.loss_continuation(id).unwrap().cause,
        ContinuationCause::Handoff
    );
    assert_eq!(loss_resumptions(&tree), 0);
    let mut replacement = await_serving_host(spawn_host(
        image_command(&sut.fixture, &sut.image)
            .envs(sut.environment.iter().cloned())
            .arg("host"),
    ))
    .unwrap();
    finish_child(&sut, &mut replacement, &child);
}
