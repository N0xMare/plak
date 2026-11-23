//! A finite handoff must not spend model grace on a joined process promotion.

use super::*;
use plakous_recursive_subagents::tree::{ProductAgentTree, TreeStore as _};
use plakous_recursive_subagents_store::SqliteTreeStore;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt as _;

struct FiniteDrain {
    _cleanup: HomeHost,
    paired: Paired,
    fixture: Fixture,
    image: PathBuf,
    environment: Vec<(&'static str, String)>,
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
        let host = start_env(&self.fixture, &[], &self.environment);
        let mut client = Client::connect(&host);
        runner::remote_ready(&mut client, &self.paired.id);
        (host, client)
    }

    fn upgrade(&self, host: &mut Host, drain: &str) {
        self.paired.advance(&self.image);
        let path = self.fixture.root.join("bounded-upgrade.log");
        let output = std::fs::File::create(&path).unwrap();
        let mut upgrade = Child::spawn(
            candidate_upgrade_with_drain(&self.fixture, &self.image, &self.environment, drain)
                .stdout(output.try_clone().unwrap())
                .stderr(output),
        );
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
        OpenOptions::new()
            .write(true)
            .custom_flags(rustix::fs::OFlags::NONBLOCK.bits().cast_signed())
            .open(self.fixture.root.join("blocked-fg.release"))
            .unwrap()
            .write_all(b"finish\n")
            .unwrap();
        wait_exit(original.0);
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
    sut.finish_job(original);
    release_continuation(&sut.fixture);
    let mut client = Client::connect(&host);
    profiles::wait_idle(&mut client, &session);
    assert_eq!(root_record(&sut.fixture, &session).1, 1);
    drop(client);
    stop(&mut host);

    let (head, _) = identity(&sut.fixture, &plakd::home::state::state_key(), &session);
    let paths = plakd::home::paths::PlakPaths::explicit(sut.fixture.home());
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
    let (mut host, mut client) = sut.start();
    let (session, parent, attachment) = sut.start_node(&mut client);
    client.prompt(3, attachment, "delegate a process", PREFIX);
    let original = sut.wait_foreground();
    drop(client);

    std::fs::write(
        &child_script,
        json!({
            "version": 1, "process_tools": true,
        "steps": [
            {"gate": {"file": sut.fixture.root.join("resume-release")},
             "output": {"tool": {"name": "wait", "arguments": {"timeout_ms": 0}}},
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
    parent_script["steps"] = json!([{"output": {"text": ANSWER}}]);
    sut.fixture.write_script(&parent_script);
    sut.upgrade(&mut host, "0");
    let deadline = Instant::now() + WAIT;
    while crash::client_control::child_dispatches(&sut.fixture) < 2 {
        assert!(
            Instant::now() < deadline,
            "the child did not resume without an attachment: {:?}",
            host.lines.try_iter().collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    sut.finish_job(original);
    release_continuation(&sut.fixture);
    root_wake::submitted_result(&sut.fixture, &parent);
    stop(&mut host);

    let paths = plakd::home::paths::PlakPaths::explicit(sut.fixture.home());
    let store = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
    let tree = ProductAgentTree::reopen(store.load(&parent.tree_id).unwrap().unwrap());
    assert_eq!(
        root_record(&sut.fixture, &session).0,
        [RootInterruptCause::OwnerLost]
    );
    let continued = tree
        .commands()
        .keys()
        .filter(|id| id.starts_with(ProductAgentTree::LOSS_RESUME_COMMAND_PREFIX))
        .count();
    assert_eq!(continued, 1);
    let locator = tree
        .nodes()
        .values()
        .find(|node| node.parent_agent_id.is_some())
        .unwrap()
        .session_locator
        .as_deref()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        inspect_process_notices(&paths.runtime_database(), locator).unwrap(),
        [1]
    );
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
    assert_eq!(marked(&sut.fixture, "blocked", "fg"), original);
}
