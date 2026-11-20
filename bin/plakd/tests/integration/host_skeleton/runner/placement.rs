//! Product topology on two real daemon homes, with the host's normal pairing registry.

use super::foreground::Paused;
use super::*;
use plakous_host_protocol::wire::{RunnerLabel, RunnerStatus};
use serde_json::Value;
use std::path::Path;

struct Pair {
    host: Fixture,
    // Owns and cleans up the second runner home independently of the first.
    _secondary: Fixture,
    launch: ChildRunner,
    secondary: ChildRunner,
    launch_id: String,
    secondary_id: String,
    copies: PathBuf,
    cuts: PathBuf,
    launch_cuts: PathBuf,
    path: String,
}

impl Pair {
    fn new() -> Self {
        let host = fixture().expect("runner fixture requires short sockets");
        let secondary_home = fixture().expect("runner fixture requires short sockets");
        git(&host.workspace(), &["init", "-q"]);
        git(
            &host.workspace(),
            &["config", "--local", "user.name", "Fixture"],
        );
        git(
            &host.workspace(),
            &["config", "--local", "user.email", "fixture@example.test"],
        );
        std::fs::write(host.workspace().join("shared.txt"), "base\n").unwrap();
        git(&host.workspace(), &["add", "shared.txt"]);
        git(&host.workspace(), &["commit", "-qm", "base"]);
        std::fs::write(host.workspace().join("draft.txt"), "live parent\n").unwrap();
        let (launch, path, launch_id) = pending_stop::remote(&host);
        let copies = secondary_home.root.canonicalize().unwrap().join("copies");
        let (secondary, _, secondary_id) =
            pending_stop::remote_with_copies(&secondary_home, Some(&copies));
        let pin = host.home().join("runners/secondary");
        std::fs::create_dir(&pin).unwrap();
        std::fs::copy(
            secondary_home.home().join("runners/remote/runner.json"),
            pin.join("runner.json"),
        )
        .unwrap();
        let config = host.home().join("config.toml");
        let base = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            &config,
            format!(
                "{base}\n[runners.secondary]\nssh = \"secondary-fixture\"\ncommand = {:?}\n",
                env!("CARGO_BIN_EXE_plakd")
            ),
        )
        .unwrap();
        let cuts = secondary_home.root.join("cuts");
        let launch_cuts = host.root.join("launch-cuts");
        std::fs::create_dir(&cuts).unwrap();
        std::fs::create_dir(&launch_cuts).unwrap();
        let proxy = host.root.join("proxy.py");
        std::fs::write(&proxy, include_str!("hold_proxy.py")).unwrap();
        let ssh = host.root.join("ssh-bin/ssh");
        let original = std::fs::read_to_string(&ssh).unwrap();
        let prefix = original.split_once("shift\nshift\n").unwrap().0;
        std::fs::write(&ssh, format!(
            "{prefix}shift\ncase \"$1\" in\n  secondary-fixture) cuts='{}'; export PLAK_HOME='{}';;\n  fixture) cuts='{}'; export PLAK_HOME='{}';;\n  *) exit 1;;\nesac\nshift\n[ ! -f \"$cuts/refuse\" ] || exit 1\n[ \"$3\" = connect ] || exec \"$@\"\nexec python3 '{}' \"$cuts\" \"$@\"",
            cuts.display(), secondary.home.display(), launch_cuts.display(), launch.home.display(), proxy.display(),
        )).unwrap();
        Self {
            host,
            _secondary: secondary_home,
            launch,
            secondary,
            launch_id,
            secondary_id,
            copies,
            cuts,
            launch_cuts,
            path,
        }
    }

    fn start(&self) -> Host {
        start_env(&self.host, &[], &[("PATH", self.path.clone())])
    }

    fn session(&self, conflict: bool, steps: &[Value]) -> (Host, Client, String, NodeAddress, u64) {
        let child = self.host.root.join("placed-child.json");
        let mut command = "test \"$(cat draft.txt)\" = 'live parent' && printf 'placed\n' > remote.txt && printf '%s\n' \"$PPID\" > process-parent".to_owned();
        if conflict {
            command.push_str(" && printf 'child\n' > shared.txt");
        }
        std::fs::write(&child, serde_json::to_vec(&json!({"version": 1, "steps": [
            {"output": {"tool": {"name": "bash", "arguments": {"command": command, "timeout": 10}}},
             "finish":"tool_calls"},
            {"output": {"tool": {"name": "submit_result", "arguments": {
                "turn_token":1,"output_json":"{\"summary\":\"placed\"}"
            }}}, "finish":"tool_calls"},
            {"output":{"text":"child done"}}
        ]})).unwrap()).unwrap();
        self.host.write_script(&json!({
            "version":1, "tree":true, "durable_tree":true, "repeat_last_step":true,
            "product_child_script":child,
            "runner_windows":{"read_ms":1500,"effect_ms":2500,"ping_ms":75,"dead_ms":500},
            "steps":steps
        }));
        let host = self.start();
        let mut client = Client::connect(&host);
        pending_stop::ready(&mut client, &self.launch_id);
        let mut request = new_session(&self.host.workspace());
        if let Op::NewSession { runner, .. } = &mut request {
            *runner = RunnerLabel {
                name: "remote".into(),
                runner_id: self.launch_id.clone(),
            };
        }
        let outcome = client.request(1, request);
        let Outcome::Ok(Answer::Created { session, node, .. }) = outcome else {
            panic!("root placement refused: {outcome:?}");
        };
        let attachment = client.attach(2, &node);
        (host, client, session, node, attachment)
    }

    fn assert_native_copy(&self, row: &Value, agent: &str) {
        let copy = Path::new(row["path"].as_str().unwrap());
        assert!(copy.starts_with(&self.copies), "{}", copy.display());
        assert_eq!(row["runner"], "secondary", "{row}");
        assert_eq!(row["agent_id"], agent);
        assert_eq!(
            std::fs::read_to_string(copy.join("remote.txt")).unwrap(),
            "placed\n"
        );
        assert_eq!(
            std::fs::read_to_string(copy.join("process-parent"))
                .unwrap()
                .trim(),
            self.secondary.pid().unwrap().to_string(),
        );
    }
}

#[path = "placement_durability.rs"]
mod durability;

fn git(path: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn tool(name: &str, arguments: &Value) -> Value {
    json!({"output":{"tool":{"name":name,"arguments":arguments}},"finish":"tool_calls"})
}

fn answer(text: &str) -> Value {
    json!({"output":{"text":text}})
}

fn spawn(runner: &str, workspace: &str) -> Value {
    tool(
        "spawn_agent",
        &json!({
            "template":"review", "tier":"mutate", "description":"placed worker", "prompt":"write remotely",
            "runner":runner,"workspace":workspace,
            "output_schema_json":"{\"type\":\"object\",\"properties\":{\"summary\":{\"type\":\"string\"}},\"required\":[\"summary\"],\"additionalProperties\":false}"
        }),
    )
}

fn wait() -> Value {
    tool(
        "wait",
        &json!({"ids":["$result.agent_id"],"timeout_ms":15000}),
    )
}

fn list() -> Value {
    tool("list_agents", &json!({"include_completed":true}))
}

fn adopt(candidate: &str, accept_conflicts: bool) -> Value {
    tool(
        "adopt_candidate",
        &json!({"candidate_id":candidate,"accept_conflicts":accept_conflicts}),
    )
}

fn tool_results(fixture: &Fixture, session: &str, count: usize) -> Vec<Value> {
    use plakd::store::session_records::SessionRecordStore as _;
    use plakous::model::{ContentBlock, ConversationItem, HistorySegment, ToolResultPart};
    use plakous::sessions::AgentSessions;
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let records = plakd::store::session_records::SqliteSessionStore::new(paths.clone());
    let mut results: Vec<Value> = Vec::new();
    wait_for(|| {
        let locator = records.load(session).unwrap().plakous_locator.unwrap();
        let Ok(history) = AgentSessions::export_history_sqlite(paths.runtime_database(), locator)
        else {
            return false;
        };
        results = history
            .segments()
            .iter()
            .flat_map(HistorySegment::items)
            .flat_map(ConversationItem::blocks)
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(result.parts()),
                _ => None,
            })
            .flatten()
            .filter_map(|part| match part {
                ToolResultPart::Text(text) => serde_json::from_str(text.text()).ok(),
                _ => None,
            })
            .collect();
        results.len() >= count
    });
    results
}

fn tree(pair: &Pair, node: &NodeAddress) -> plakous_hosted::agent_tree::ProductAgentTree {
    let paths = plakd::home::paths::PlakPaths::explicit(pair.host.home());
    plakd::store::tree::load_tree(&paths, &node.tree_id)
        .unwrap()
        .unwrap()
}

#[test]
fn cross_runner_child_fork_runs_holds_seals_and_adopts() {
    let pair = Pair::new();
    write_cut_marker(&pair.cuts.join("refuse"), b"refuse");
    let (mut host, mut client, session, node, attachment) = pair.session(
        false,
        &[
            spawn("missing", "fork"),
            spawn("secondary", "inherit"),
            spawn("secondary", "fork"),
            answer("refusals checked"),
            spawn(&pair.secondary_id, "fork"),
            answer("child assigned"),
            answer("root continued independently"),
            wait(),
            list(),
            adopt("$result.candidate", false),
            answer("child adopted"),
        ],
    );
    client.prompt(3, attachment, "check refusals", "refusals checked");
    let results = tool_results(&pair.host, &session, 3);
    let codes = results
        .iter()
        .filter_map(|result| result["code"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        codes,
        [
            "runner_not_configured",
            "runner_mismatch",
            "runner_unreachable"
        ]
    );
    assert_eq!(tree(&pair, &node).nodes().len(), 1);
    std::fs::remove_file(pair.cuts.join("refuse")).unwrap();
    pending_stop::ready(&mut client, &pair.secondary_id);
    write_cut_marker(&pair.cuts.join("before-answer.armed"), b"armed");
    client.prompt(4, attachment, "start the placed child", "child assigned");
    let results = tool_results(&pair.host, &session, 4);
    let receipt = &results[3];
    assert_eq!(receipt["runner"], pair.secondary_id, "{receipt}");
    let agent = receipt["agent_id"].as_str().unwrap();
    wait_for(|| pair.cuts.join("before-answer.entered").is_file());
    let mut paused = Paused::new(pair.secondary.pid().unwrap());
    wait_for(|| {
        let row = client
            .rows(15)
            .into_iter()
            .find(|row| row.session == session)
            .unwrap();
        assert_eq!(row.runner_status, RunnerStatus::Ready);
        row.needs_operator
    });
    assert!(tree(&pair, &node).candidates().is_empty());
    client.prompt(
        17,
        attachment,
        "root can work during the hold",
        "root continued independently",
    );
    let child_dispatches = plakous_offline_crash_harness::evidence::read_evidence(
        pair.host.root.join("child-evidence.jsonl"),
    )
    .unwrap()
    .into_iter()
    .filter(|event| {
        event.kind == plakous_offline_crash_harness::evidence::EvidenceKind::ProviderDispatched
    })
    .count();
    assert_eq!(child_dispatches, 1);
    paused.resume();
    write_cut_marker(&pair.cuts.join("before-answer.release"), b"release");
    client.prompt(18, attachment, "wait and adopt", "child adopted");
    let results = tool_results(&pair.host, &session, 7);
    let row = &results[5]["agents"][0];
    pair.assert_native_copy(row, agent);
    let adopted = &results[6];
    assert!(
        adopted["files_changed"]
            .as_u64()
            .is_some_and(|count| count >= 1),
        "{results:?}"
    );
    assert_eq!(
        std::fs::read_to_string(pair.host.workspace().join("remote.txt")).unwrap(),
        "placed\n"
    );
    stop(&mut host);

    reopen_placed_child(
        &pair,
        &session,
        &node,
        agent,
        Path::new(row["path"].as_str().unwrap()),
    );
}

fn script_followup(pair: &Pair, agent: &str) {
    let mut root_script: Value =
        serde_json::from_slice(&std::fs::read(pair.host.script()).unwrap()).unwrap();
    let child = Path::new(root_script["product_child_script"].as_str().unwrap());
    let mut child_script: Value = serde_json::from_slice(&std::fs::read(child).unwrap()).unwrap();
    child_script["steps"][0]["output"]["tool"]["arguments"]["command"] =
        json!("printf '%s\\n' \"$PPID\" > reopen-parent");
    child_script["steps"][1]["output"]["tool"]["arguments"]["turn_token"] = json!(2);
    std::fs::write(child, serde_json::to_vec(&child_script).unwrap()).unwrap();
    root_script["steps"] = json!([
        tool(
            "send_agent_message",
            &json!({"agent_id":agent,"purpose":"followup","message":"prove the saved placement","refresh":false})
        ),
        tool("wait", &json!({"ids":[agent],"timeout_ms":15000})),
        answer("reopened child ran")
    ]);
    pair.host.write_script(&root_script);
}

fn reopen_placed_child(pair: &Pair, session: &str, node: &NodeAddress, agent: &str, path: &Path) {
    script_followup(pair, agent);
    let mut host = pair.start();
    let mut client = Client::connect(&host);
    pending_stop::ready(&mut client, &pair.launch_id);
    pending_stop::ready(&mut client, &pair.secondary_id);
    let root = client.attach(106, node);
    let saved = tree(pair, node);
    let placed = saved.node(agent).unwrap();
    assert_eq!(
        placed.placement.runner_id.as_deref(),
        Some(pair.secondary_id.as_str())
    );
    assert_eq!(placed.path, path);
    assert!(
        saved
            .candidates()
            .values()
            .any(|candidate| candidate.adopted_by.is_some())
    );
    client.prompt(107, root, "resume the placed child", "reopened child ran");
    let results = tool_results(&pair.host, session, 9);
    assert!(results[7]["disposition"].is_string(), "{results:?}");
    assert_eq!(
        std::fs::read_to_string(path.join("reopen-parent"))
            .unwrap()
            .trim(),
        pair.secondary.pid().unwrap().to_string(),
    );
    stop(&mut host);
}

#[test]
fn frozen_template_defaults_to_a_peer_and_explicit_local_overrides_it() {
    let pair = Pair::new();
    let agents = pair.host.home().join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("review.toml"),
        "account = \"openrouter\"\nmodel = \"offline/child\"\ndescription = \"Fixture card\"\nrunner = \"secondary\"\n",
    ).unwrap();
    let mut from_card = spawn("secondary", "fork");
    from_card["output"]["tool"]["arguments"]
        .as_object_mut()
        .unwrap()
        .remove("runner");
    let (mut host, mut client, session, node, attachment) = pair.session(
        false,
        &[
            from_card,
            wait(),
            answer("template applied"),
            spawn("local", "inherit"),
            wait(),
            answer("override applied"),
        ],
    );
    pending_stop::ready(&mut client, &pair.secondary_id);
    client.prompt(11, attachment, "use the frozen default", "template applied");
    client.prompt(
        12,
        attachment,
        "use the parent's placement",
        "override applied",
    );
    let results = tool_results(&pair.host, &session, 4);
    assert_eq!(results[0]["runner"], pair.secondary_id);
    assert_eq!(results[2]["runner"], pair.launch_id);
    let tree = tree(&pair, &node);
    let remote = tree.node(results[0]["agent_id"].as_str().unwrap()).unwrap();
    let retained = tree.node(results[2]["agent_id"].as_str().unwrap()).unwrap();
    assert!(remote.path.starts_with(&pair.copies));
    assert_eq!(retained.path, tree.node("root").unwrap().path);
    stop(&mut host);
}

#[test]
fn cross_runner_child_retains_text_conflict_on_parent_runner() {
    let pair = Pair::new();
    let (mut host, mut client, session, node, attachment) = pair.session(
        true,
        &[
            spawn("secondary", "fork"),
            wait(),
            list(),
            answer("candidate sealed"),
            adopt("$result.candidate", false),
            answer("conflict handled"),
        ],
    );
    client.prompt(3, attachment, "spawn and seal", "candidate sealed");
    let results = tool_results(&pair.host, &session, 3);
    let agent = results[0]["agent_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{results:?}"));
    pair.assert_native_copy(&results[2]["agents"][0], agent);
    let shared = pair.host.workspace().join("shared.txt");
    std::fs::write(&shared, "parent\n").unwrap();
    client.prompt(
        4,
        attachment,
        "adopt against changed target",
        "conflict handled",
    );
    let results = tool_results(&pair.host, &session, 4);
    assert_eq!(results[3]["code"], "merge_conflict", "{results:?}");
    assert_eq!(std::fs::read_to_string(&shared).unwrap(), "parent\n");
    let saved = tree(&pair, &node);
    let conflict = saved
        .candidates()
        .values()
        .find(|candidate| candidate.kind == plakous_hosted::agent_tree::CandidateKind::Conflict)
        .expect("cross-runner conflict pins a conflict candidate");
    git(
        &pair.host.workspace(),
        &["cat-file", "-e", &format!("{}:shared.txt", conflict.commit)],
    );

    // A fresh offline script represents the parent's explicit decision on the retained id.
    let mut script: Value =
        serde_json::from_slice(&std::fs::read(pair.host.script()).unwrap()).unwrap();
    script["steps"] = json!([
        adopt(conflict.candidate_id.as_str(), true),
        answer("markers accepted")
    ]);
    stop(&mut host);
    pair.host.write_script(&script);
    let mut host = pair.start();
    let mut client = Client::connect(&host);
    pending_stop::ready(&mut client, &pair.launch_id);
    pending_stop::ready(&mut client, &pair.secondary_id);
    let attachment = client.attach(2, &node);
    client.prompt(
        3,
        attachment,
        "accept the retained markers",
        "markers accepted",
    );
    let results = tool_results(&pair.host, &session, 5);
    assert!(results[4]["files_changed"].is_number(), "{results:?}");
    assert!(
        std::fs::read_to_string(&shared)
            .unwrap()
            .contains("<<<<<<<")
    );
    stop(&mut host);
}

#[track_caller]
fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + WAIT;
    while !predicate() {
        assert!(
            std::time::Instant::now() < deadline,
            "runner placement did not converge"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
