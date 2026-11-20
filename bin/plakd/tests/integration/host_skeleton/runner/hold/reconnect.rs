use super::*;
use plakd::store::session_records::SessionRecordStore as _;
use plakous::sessions::{AgentSessions, SessionBackend, SessionLocator, SessionTranscriptPart};
use plakous_host_protocol::topology::NodeStatus;
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};

fn root_call(command: &str) -> serde_json::Value {
    json!({"name":"process_session_start","arguments":{
        "command":command, "workdir":".","login":false,"tty":false,
        "keep_stdin_open":false,"yield_time_ms":1000,"max_output_bytes":8192
    }})
}

fn script_reconnecting_turns(fixture: &Fixture, remote: &Remote) {
    let root_gate = fixture.root.join("root-release");
    let child_gate = fixture.root.join("child-release");
    let root_marker = fixture.root.join("root-effect");
    let child_marker = fixture.root.join("child-effect");
    let child_script = fixture.root.join("child-script.json");
    std::fs::write(
        fixture.root.join("grandchild-script.json"),
        r#"{"version":1,"steps":[]}"#,
    )
    .unwrap();
    std::fs::write(
        &child_script,
        json!({"version":1,"steps":[
            {"stream":[{"gate":{"file":child_gate}}],"output":{"tool":{
                "name":"bash","arguments":{
                    "command":format!("printf wrong >> '{}'",child_marker.display())
                }
            }},"finish":"tool_calls"},
            {"output":{"tool":{
                "name":"bash","arguments":{
                    "command":format!("printf child >> '{}'",child_marker.display())
                }
            }},"finish":"tool_calls"},
            {"output":{"tool":{"name":"submit_result","arguments":{
                "turn_token":1,"output_json":"{\"summary\":\"continued\"}"
            }}},"finish":"tool_calls"},
            {"output":{"text":"child finished"}}
        ]})
        .to_string(),
    )
    .unwrap();
    let mut script = spawning_script();
    script["product_child_script"] = json!(child_script);
    script["tree_process_tools"] = json!(true);
    script["runner_windows"] = remote.script(fixture)["runner_windows"].clone();
    script["runner_windows"]["read_ms"] = json!(5000);
    script["runner_hold_cuts"] = json!(remote.cuts);
    let spawn = script["steps"][0].take();
    script["steps"] = json!([
        spawn,
        {"stream":[{"gate":{"file":root_gate}}],"output":{"tool":root_call(
            &format!("printf wrong >> '{}'",root_marker.display())
        )},"finish":"tool_calls"},
        {"output":{"tool":root_call(&format!("printf root >> '{}'",root_marker.display()))},
            "finish":"tool_calls"},
        {"output":{"tool":{"name":"wait_agent","arguments":{
            "agent_ids":["c1"]
        }}},"finish":"tool_calls"},
        {"output":{"text":ANSWER}}
    ]);
    fixture.write_script(&script);
}

fn refusal(
    executor: &tokio::runtime::Runtime,
    sessions: &AgentSessions,
    locator: SessionLocator,
) -> Option<String> {
    let transcript = executor
        .block_on(sessions.transcript_source(locator))
        .unwrap();
    transcript
        .tail(32, 16_384)
        .items()
        .iter()
        .flat_map(plakous::sessions::SessionTranscriptItem::parts)
        .find_map(|part| {
            let SessionTranscriptPart::ToolResult { observation, .. } = part else {
                return None;
            };
            let preview = observation.preview();
            preview.contains("identity").then(|| preview.to_owned())
        })
}

fn cut_final_probes(
    fixture: &Fixture,
    remote: &Remote,
    session: &str,
    child: SessionLocator,
) -> (String, [String; 2]) {
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let root = plakd::store::session_records::SqliteSessionStore::new(paths.clone())
        .load(session)
        .unwrap()
        .plakous_locator
        .unwrap();
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let bridge = std::fs::read_to_string(fixture.root.join("ssh-bridges")).unwrap();
    let pid = bridge.lines().last().unwrap().parse().unwrap();
    write_cut_marker(&remote.cuts.join("final-probe.armed"), b"armed");
    for (gate, marker) in [
        ("root-release", "final-probe.1"),
        ("child-release", "final-probe.2"),
    ] {
        std::fs::write(fixture.root.join(gate), b"continue").unwrap();
        wait_file(&remote.cuts.join(marker));
    }
    std::fs::remove_file(remote.cuts.join("final-probe.armed")).unwrap();
    for name in ["before-ready.armed", "redial.held"] {
        write_cut_marker(&remote.cuts.join(name), b"held");
    }
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();

    let refusals = std::cell::OnceCell::new();
    until_within(
        "both tool refusals before redial",
        std::time::Duration::from_secs(8),
        || {
            if let (Some(root), Some(child)) = (
                refusal(&executor, &sessions, root),
                refusal(&executor, &sessions, child),
            ) {
                refusals.set([root, child]).unwrap();
                true
            } else {
                false
            }
        },
    );
    assert_eq!(providers(fixture), 2, "no later round during outage");
    (bridge, refusals.into_inner().unwrap())
}

#[test]
fn same_daemon_link_reconnect_keeps_live_root_and_child_and_releases_child_lease() {
    let fixture = strict_fixture().with_session_lease_seconds(3);
    let remote = Remote::new(&fixture);
    script_reconnecting_turns(&fixture, &remote);
    let (mut host, _attached, session, root) = remote.start_with(
        &fixture,
        &[(
            "PLAK_TEST_FINAL_RUNNER_PROBE",
            remote.cuts.to_str().unwrap().to_owned(),
        )],
    );
    let mut client = Client::connect(&host);
    wait_file(&fixture.root.join("evidence.jsonl"));
    until("root and child provider streams in flight", || {
        providers(&fixture) == 2
            && read_evidence(fixture.root.join("child-evidence.jsonl")).is_ok_and(|events| {
                events
                    .iter()
                    .any(|event| event.kind == EvidenceKind::ProviderDispatched)
            })
    });
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let child = || {
        plakd::store::tree::load_tree(&paths, &root.tree_id)
            .unwrap()
            .unwrap()
            .nodes()["c1"]
            .clone()
    };
    let before = child();
    assert_eq!(before.status, NodeStatus::Running);
    assert!(before.active_turn);
    assert_eq!(
        before.placement.runner_id.as_deref(),
        Some(remote.id.as_str())
    );
    let daemon = remote.runner.pid().unwrap();
    let locator: SessionLocator = before.session_locator.unwrap().parse().unwrap();
    let (bridge, refusals) = cut_final_probes(&fixture, &remote, &session, locator);
    write_cut_marker(&remote.cuts.join("redial.release"), b"released");
    remote.entered("before-ready");
    assert_eq!(remote.runner.pid(), Some(daemon), "runner did not restart");
    assert_ne!(
        std::fs::read_to_string(fixture.root.join("ssh-bridges")).unwrap(),
        bridge
    );
    assert_eq!(held(&mut client, &session).activity, SessionActivity::Busy);
    assert_eq!(child().attempt_id, before.attempt_id);
    let root_marker = fixture.root.join("root-effect");
    let child_marker = fixture.root.join("child-effect");
    assert!(!root_marker.exists() && !child_marker.exists());

    remote.release("before-ready");
    pending_stop::ready(&mut client, &remote.id);
    wait_file(&root_marker);
    wait_file(&child_marker);
    idle(&mut client, &session);
    let database = rusqlite::Connection::open(paths.runtime_database()).unwrap();
    until("child retired and released its branch lease", || {
        let node = child();
        assert_ne!(node.status, NodeStatus::RecoveryRequired);
        let released: bool = database.query_row(
            "SELECT active_owner IS NULL AND released_owner IS NOT NULL FROM branches WHERE session_id=?1 AND branch_id=?2",
            [locator.session_id().to_string(), locator.branch_id().to_string()],
            |row| row.get(0),
        ).unwrap();
        node.status == NodeStatus::Completed && released
    });
    assert_eq!(child().attempt_id, before.attempt_id);
    assert_eq!(snapshot(&fixture, &session).terminals, 1);
    assert_eq!(unknown_effects(&fixture, &session), 0);
    let log = std::fs::read_to_string(fixture.home().join("logs/plak.log")).unwrap();
    assert!(!log.contains("lease-renew gate exceeded"));
    assert_eq!(std::fs::read(root_marker).unwrap(), b"root");
    assert_eq!(std::fs::read(child_marker).unwrap(), b"child");
    let requests = std::fs::read_to_string(remote.cuts.join("wire.jsonl")).unwrap();
    let starts = requests
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|row| row["verb"] == "process.start" && row["replay"] == false);
    assert_eq!(
        starts.count(),
        2,
        "both tools ran through the paired daemon"
    );
    assert_eq!(
        client.request(72, Op::ReleaseSession { session }),
        Outcome::Ok(Answer::Released)
    );
    stop(&mut host);
    for refusal in refusals {
        assert!(
            refusal.contains("runner workspace identity is unavailable"),
            "outage was misclassified: {refusal}"
        );
    }
}
