//! The cut path against a literal production predecessor, without upgrade overrides.

use super::*;
use crate::gateway_process::GatewayProcess;
use plakd::home::paths::PlakPaths;
use plakd::store::session_records::{SessionRecordStore as _, SqliteSessionStore};
use plakd_wire_types::lifecycle::{HostLifecyclePhase, HostLifecycleReason};
use plakous_offline_crash_harness::ledger_signature::{
    inspect_branch_checkpoint, inspect_process_notices, inspect_root_lifecycles,
};
use plakous_recursive_subagents::{
    protocol::MessagePurpose,
    tree::{ProductAgentTree, TreeStore as _},
};
use plakous_recursive_subagents_store::SqliteTreeStore;
use serde_json::Value;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

mod browser;
mod source;
use browser::Browser;

const HISTORY: [&str; 3] = [
    "first retained reply",
    "second retained reply",
    "runner started",
];
const STREAM: &str = "predecessor streaming";
const FINAL: &str = "predecessor stream completed";
const RESUMED: &str = "candidate reply";
const DIFFERENT_STATE_KEY: &str = "host_upgrade_offline_migration_required";

#[test]
fn production_predecessor_with_a_missing_child_evaluator_can_upgrade() {
    let old = PathBuf::from(std::env::var_os("PLAK_INSTALL_PREDECESSOR").unwrap()).join("plakd");
    let candidate = PathBuf::from(std::env::var_os("PLAK_TEST_BINARY").unwrap());
    let (fixture, _) = root_wake::child_fixture(false).expect("short fixture socket paths");
    let (before, after) = probe_pair(&fixture, &old, &candidate);
    if before["state_key"] == after["state_key"] {
        upgrade::reopen::children::missing_evaluator_upgrade(
            &fixture,
            &old,
            &candidate,
            |session| {
                source::idle_locator(&source::open(&fixture), session, 1);
            },
        );
    }
}

#[test]
fn production_predecessor_refuses_a_destination_only_unreadable_child() {
    let old = PathBuf::from(std::env::var_os("PLAK_INSTALL_PREDECESSOR").unwrap()).join("plakd");
    let candidate = PathBuf::from(std::env::var_os("PLAK_TEST_BINARY").unwrap());
    let (fixture, _) = root_wake::child_fixture(false).expect("short fixture socket paths");
    let (before, after) = probe_pair(&fixture, &old, &candidate);
    if before["state_key"] != after["state_key"] {
        return;
    }
    let _cleanup = upgrade::HomeHost(fixture.home());
    let mut host =
        await_serving_host(spawn_host(image_command(&fixture, &old).arg("host"))).unwrap();
    let (mut client, _, node) =
        upgrade::reopen::children::spawn_child_with_idle(&fixture, &host, |session| {
            source::idle_locator(&source::open(&fixture), session, 1);
        });
    let id = after["binary_id"].as_str().unwrap();
    let result = image_command(&fixture, &candidate)
        .env("PLAK_TEST_CHILD_BINDING_UNREADABLE_IMAGE", &candidate)
        .args(["host", "upgrade", "--expect-binary", id, "--drain", "0"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success(), "{stderr}");
    assert!(
        stderr.contains("host_upgrade_child_reopen_refused: 1 affected child session(s)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("child binding is unreadable in this image"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("resolving earlier admission verdicts"),
        "{stderr}"
    );
    assert!(matches!(
        client.request(4, new_session(&fixture.workspace())),
        Outcome::Ok(Answer::Created { .. })
    ));
    std::fs::write(fixture.root.join("release-child"), []).unwrap();
    root_wake::submitted_result(&fixture, &node);
    stop(&mut host);
}

#[test]
fn production_predecessor_upgrade_preserves_live_work() {
    let directory = std::env::var_os("PLAK_INSTALL_PREDECESSOR")
        .expect("prepare the pinned predecessor with tools/gate/build_predecessor.py");
    let old = PathBuf::from(directory).join("plakd");
    let candidate = PathBuf::from(
        std::env::var_os("PLAK_TEST_BINARY").expect("install profile requires the candidate build"),
    );
    let (fixture, script) = root_wake::child_fixture(true).expect("short fixture socket paths");
    let workspace = fixture.root.join("local");
    std::fs::create_dir(&workspace).unwrap();
    let mut paired = Paired::new(&fixture, &old);
    let socket = configure(&fixture, &paired);
    paired.pair(&fixture, &old);

    let (old_facts, new_facts) = probe_pair(&fixture, &old, &candidate);
    let new_id = new_facts["binary_id"].as_str().unwrap();
    let same_key = old_facts["state_key"] == new_facts["state_key"];
    if same_key {
        seed_script(&fixture, &script);
    } else {
        let mut script = script;
        script["steps"] = json!([stream_step(&fixture)]);
        fixture.write_script(&script);
    }

    run(fixture
        .base_command_from(&old)
        .args(["mcp", "test", "fake"]));
    let mut gateway = start_gateway(&fixture, &old);
    let old_service = gateway_check(&fixture, &old);
    assert_eq!(gateway_check(&fixture, &candidate), old_service);
    let mut local = start_local_runner(&fixture, &old);
    let runner_pids = (local.id(), paired.daemon.id());
    let mut host = start_host(&fixture, &old, &paired, &socket, &workspace);
    runner::wait_local_runner_ready(&host);
    let pid = host.child.as_ref().unwrap().id();
    let inode = std::fs::symlink_metadata(&socket).unwrap().ino();
    let mut client = Client::connect(&host);
    paired.ready(&mut client);
    let login = crate::web_endpoint::handoff_cookie(&socket);
    let mut web = Browser::connect(&socket, &login);
    let mut upgrade = upgrade_command(&fixture, &candidate, &paired, new_id);
    if !same_key {
        let (_, node) = create_root(&mut client, &workspace);
        refuse_changed_key(&fixture, &mut upgrade, &mut client, &mut web, &node);
        stop(&mut host);
        return;
    }

    let document = fixture.root.join("predecessor-export.json");
    let root = seed(&fixture, &mut client, &workspace, &old, &document);
    let prior = lifecycles(&fixture, &root);
    assert_terminals(&prior, HISTORY.len());
    assert!(
        std::fs::read_to_string(fixture.root.join("mcp-list.log"))
            .unwrap()
            .lines()
            .any(|line| line == "list"),
        "the predecessor did not negotiate tools with the offline MCP server"
    );
    let original_job = check_job(&fixture, i32::try_from(runner_pids.0).unwrap());

    let old_generation = web.welcome.host_generation;
    web.attach(&root.node);
    web.prompt("keep this websocket streaming during the cut");
    web.wait_for_text(STREAM);
    eventually("source child model did not park", || {
        child_dispatches(&fixture) == 1
    });

    run(fixture.base_command_from(&old).args(["gateway", "drain"]));
    gateway.wait_for_drain();
    let _gateway = start_gateway(&fixture, &candidate);
    let new_service = gateway_check(&fixture, &old);
    assert_ne!(
        old_service, new_service,
        "the gateway instance did not change"
    );
    assert_eq!(gateway_check(&fixture, &candidate), new_service);

    paired.advance(&candidate);
    handoff(&fixture, upgrade.arg("--backup-store"), &mut web, new_id);
    assert_host_reexec(&mut host, &socket, pid, inode);
    assert_private_backup(&fixture);

    let mut web = Browser::connect(&socket, &login);
    assert_ne!(web.welcome.host_generation, old_generation);
    assert_eq!(web.welcome.binary_id, new_id);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    paired.ready(&mut client);
    assert_eq!((local.id(), paired.daemon.id()), runner_pids);
    assert!(
        local.try_wait().unwrap().is_none(),
        "old local daemon exited"
    );
    assert!(
        paired.daemon.try_wait().unwrap().is_none(),
        "old paired daemon exited"
    );
    std::fs::write(fixture.root.join("finish-stream"), []).unwrap();
    verify_resumed_root(&fixture, &mut web, &root, &prior, pid, original_job);
    let imported = import(&fixture, &candidate, &workspace, &document);
    assert_ne!(imported, root.id);
    assert_imported_history(&fixture, &imported);
    drop(web);
    stop(&mut host);
    assert_child_continued_once(&fixture, &root.node);
}

fn assert_private_backup(fixture: &Fixture) {
    let paths = PlakPaths::explicit(fixture.home());
    let mut backups = std::fs::read_dir(paths.state_dir().join("upgrade-backups")).unwrap();
    let backup = backups.next().unwrap().unwrap().path();
    assert!(backups.next().is_none());
    for name in ["product.sqlite3", "subagents.sqlite3", "runtime.sqlite3"] {
        let meta = std::fs::symlink_metadata(backup.join(name)).unwrap();
        assert!(meta.is_file() && meta.permissions().mode() & 0o777 == 0o600);
    }
}

#[test]
fn a_serving_predecessor_blocks_upgrade_verdict_acknowledgement() {
    let old = PathBuf::from(std::env::var_os("PLAK_INSTALL_PREDECESSOR").unwrap()).join("plakd");
    let candidate = PathBuf::from(std::env::var_os("PLAK_TEST_BINARY").unwrap());
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    let (before, after) = probe_pair(&fixture, &old, &candidate);
    if before["state_key"] != after["state_key"] {
        return;
    }
    fixture.start_gateway_with(&[]);
    let mut host =
        await_serving_host(spawn_host(fixture.base_command_from(&old).arg("host"))).unwrap();
    // An interrupted exec can be followed by a manager start through the
    // unchanged service link. The old image cannot see the durable fence.
    let path = fixture.home().join("host/upgrade-checkpoint-verdict");
    let id = uuid::Uuid::new_v4().to_string();
    let pending = json!({
        "version":1, "state_key": after["state_key"], "nonce": id,
        "runtime_expected": false, "outcome": "Pending"
    })
    .to_string();
    write_private(&path, &pending);
    let mut recovery = fixture.base_command_from(&candidate);
    recovery.args(["host", "upgrade-verdict", "--acknowledge", &id]);
    let refused = recovery.output().unwrap();
    assert!(
        !refused.status.success()
            && String::from_utf8_lossy(&refused.stderr)
                .contains("host_upgrade_acknowledge_requires_stopped_home"),
        "{refused:?}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), pending);
    assert_eq!(
        Client::connect(&host).request(1, Op::Ping),
        Outcome::Ok(Answer::Pong)
    );
    stop(&mut host);
    let missing = recovery.output().unwrap();
    assert!(
        !missing.status.success()
            && String::from_utf8_lossy(&missing.stderr)
                .contains("host_upgrade_acknowledge_baseline_unproven"),
        "{missing:?}"
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), pending);
}

#[test]
fn production_predecessor_never_observes_ready_after_destination_preservation_refusal() {
    let directory = std::env::var_os("PLAK_INSTALL_PREDECESSOR").unwrap();
    let old = PathBuf::from(directory).join("plakd");
    let candidate = PathBuf::from(std::env::var_os("PLAK_TEST_BINARY").unwrap());
    let fixture = Fixture::new(&json!({"version": 1, "steps": []}));
    let _cleanup = upgrade::HomeHost(fixture.home());
    let (old_facts, new_facts) = probe_pair(&fixture, &old, &candidate);
    if old_facts["state_key"] != new_facts["state_key"] {
        return;
    }
    fixture.start_gateway_with(&[]);
    let id = new_facts["binary_id"].as_str().unwrap();
    let gate = fixture.root.join("hold-exec");
    std::fs::write(&gate, []).unwrap();
    let mut host = await_serving_host(spawn_host(
        fixture
            .base_command_from(&old)
            .env("PLAK_TEST_HOST_BEFORE_PAIRED_EXEC", &gate)
            .arg("host"),
    ))
    .unwrap();
    let mut child = Child::spawn(
        fixture
            .base_command_from(&candidate)
            .args(["host", "upgrade", "--expect-binary", id, "--drain", "0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    let deadline = Instant::now() + WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = host
            .lines
            .recv_timeout(remaining)
            .expect("source never reached exec");
        if line.contains("waiting at paired exec gate") {
            break;
        }
    }
    upgrade::preservation::replace_store_identity(&fixture);
    std::fs::remove_file(gate).unwrap();
    eventually("gated requester did not report failure", || {
        child.try_wait().unwrap().is_some()
    });
    let result = child.wait().unwrap();
    let mut detail = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut detail).unwrap();
    assert_eq!(result.code(), Some(5), "{detail}");
    assert!(
        detail.contains(
            "host_upgrade_fix_forward_required: destination preservation failed after surrender"
        ),
        "{detail}"
    );
    let status = host.child.take().unwrap().wait().unwrap();
    // Restart-on-failure must not relaunch the predecessor's service link.
    assert!(status.success());
    assert!(!fixture.home().join("host/upgrade.sock").exists());
}

fn assert_host_reexec(host: &mut Host, socket: &Path, pid: u32, inode: u64) {
    let child = host.child.as_mut().unwrap();
    assert_eq!(child.id(), pid);
    assert!(child.try_wait().unwrap().is_none());
    assert_eq!(std::fs::symlink_metadata(socket).unwrap().ino(), inode);
}

fn upgrade_command(fixture: &Fixture, candidate: &Path, paired: &Paired, id: &str) -> Command {
    let mut command = fixture.base_command_from(candidate);
    command.env("PATH", &paired.path).args([
        "host",
        "upgrade",
        "--expect-binary",
        id,
        "--drain",
        "0",
    ]);
    command
}

fn refuse_changed_key(
    fixture: &Fixture,
    upgrade: &mut Command,
    client: &mut Client,
    web: &mut Browser,
    node: &NodeAddress,
) {
    web.attach(node);
    web.prompt("stream on the source while the other key is refused");
    web.wait_for_text(STREAM);
    let refused = upgrade.output().unwrap();
    assert!(!refused.status.success(), "{refused:?}");
    let detail = String::from_utf8_lossy(&refused.stderr);
    assert!(
        detail.lines().any(|line| line
            .strip_prefix("plakd: ")
            .and_then(|error| error.split_once(':'))
            .is_some_and(|(code, _)| code == DIFFERENT_STATE_KEY)),
        "{detail}"
    );
    assert_eq!(client.request(40, Op::Ping), Outcome::Ok(Answer::Pong));
    assert!(!fixture.home().join("host/upgrades").exists());
    std::fs::write(fixture.root.join("finish-stream"), []).unwrap();
    web.expect_completed();
}

fn handoff(fixture: &Fixture, upgrade: &mut Command, web: &mut Browser, new_id: &str) {
    let log_path = fixture.root.join("upgrade.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut upgrading = Child::spawn(upgrade.stdout(log.try_clone().unwrap()).stderr(log));
    let mut check_upgrade = || {
        if let Some(status) = upgrading.try_wait().unwrap() {
            assert!(
                status.success(),
                "{}",
                std::fs::read_to_string(&log_path).unwrap()
            );
        }
    };
    assert_eq!(
        web.lifecycle(HostLifecyclePhase::Draining, new_id, &mut check_upgrade),
        0
    );
    let terminals = web.lifecycle(HostLifecyclePhase::Restarting, new_id, &mut check_upgrade);
    assert!(
        terminals + web.expect_closed() <= 1,
        "duplicate source terminal"
    );
    eventually("candidate upgrade CLI did not exit", || {
        upgrading.try_wait().unwrap().is_some()
    });
    assert!(
        upgrading.wait().unwrap().success(),
        "{}",
        std::fs::read_to_string(log_path).unwrap()
    );
}

fn verify_resumed_root(
    fixture: &Fixture,
    web: &mut Browser,
    root: &Root,
    prior: &[Value],
    pid: u32,
    original_job: (i32, i32),
) {
    web.attach(&root.node);
    web.snapshot_contains(&[HISTORY[0], HISTORY[1]]);
    assert_eq!(custody_pid(fixture, &root.node.tree_id), Some(pid));
    let shells = std::fs::read_to_string(fixture.root.join("shell-invocations")).unwrap();
    let pid_text = pid.to_string();
    assert_eq!(shells.lines().filter(|row| *row == pid_text).count(), 1);
    let reopened = locator(fixture, &root.id);
    assert_eq!(reopened.session_id(), root.locator.session_id());
    assert_eq!(reopened.branch_id(), root.locator.branch_id());
    root_wake::idle_turns(fixture, &root.id, 4);
    let finished = lifecycles(fixture, root);
    assert_terminals(&finished, HISTORY.len() + 1);
    assert_eq!(&finished[..prior.len()], prior);

    assert_eq!(check_job(fixture, original_job.1), original_job);
    std::fs::write(fixture.root.join("job.release"), []).unwrap();
    assert_retained_output(fixture, root);
    eventually("candidate did not continue the child", || {
        child_dispatches(fixture) == 2
    });
    std::fs::write(fixture.root.join("release-child"), []).unwrap();
    root_wake::submitted_result(fixture, &root.node);
}

fn assert_child_continued_once(fixture: &Fixture, node: &NodeAddress) {
    let paths = PlakPaths::explicit(fixture.home());
    let store = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
    let tree = ProductAgentTree::reopen(store.load(&node.tree_id).unwrap().unwrap());
    assert_eq!(
        tree.commands()
            .keys()
            .filter(|id| id.starts_with(ProductAgentTree::LOSS_RESUME_COMMAND_PREFIX))
            .count(),
        1,
        "the successor must schedule exactly one continuation"
    );
    assert_eq!(
        tree.messages()
            .values()
            .filter(|mail| mail.purpose == MessagePurpose::Result && mail.to == tree.root_agent_id)
            .count(),
        1,
        "the continued child must deliver exactly one result"
    );
}

fn import(fixture: &Fixture, candidate: &Path, workspace: &Path, document: &Path) -> String {
    let imported = run(fixture
        .base_command_from(candidate)
        .current_dir(workspace)
        .args(["session", "import"])
        .arg(document));
    imported
        .lines()
        .find_map(|line| line.strip_prefix("resume with: plakd run --session "))
        .unwrap()
        .to_owned()
}

fn assert_imported_history(fixture: &Fixture, session: &str) {
    let db = PlakPaths::explicit(fixture.home()).runtime_database();
    let checkpoint = inspect_branch_checkpoint(&db, locator(fixture, session)).unwrap();
    let history = checkpoint.manifest().canonical().history();
    let text = history
        .segments()
        .iter()
        .flat_map(plakous::model::HistorySegment::items)
        .flat_map(plakous::model::ConversationItem::blocks)
        .filter_map(|block| match block {
            plakous::model::ContentBlock::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        text.contains(&HISTORY[0]) && text.contains(&HISTORY[1]) && !text.contains(&HISTORY[2]),
        "import did not preserve the checkpoint exported before the cut: {text:?}"
    );
}

fn child_dispatches(fixture: &Fixture) -> usize {
    if fixture.root.join("child-evidence.jsonl").exists() {
        crash::client_control::child_dispatches(fixture)
    } else {
        0
    }
}

fn run(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{} {:?}: {}",
        Path::new(command.get_program()).display(),
        command.get_args(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn eventually(detail: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(Instant::now() < deadline, "{detail}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn probe(fixture: &Fixture, binary: &Path) -> Value {
    serde_json::from_str(&run(fixture
        .base_command_from(binary)
        .args(["version", "--host-upgrade-probe"])))
    .unwrap()
}

fn probe_pair(fixture: &Fixture, old: &Path, candidate: &Path) -> (Value, Value) {
    let old_facts = probe(fixture, old);
    let new_facts = probe(fixture, candidate);
    let pin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.config/install-predecessor");
    assert_eq!(
        old_facts["runner_version"]["source_revision"],
        std::fs::read_to_string(pin).unwrap().trim()
    );
    assert_ne!(
        old_facts["binary_id"], new_facts["binary_id"],
        "the test requires two distinct executables"
    );
    assert_ne!(old_facts["runner_version"], new_facts["runner_version"]);
    (old_facts, new_facts)
}

struct Paired {
    id: String,
    path: String,
    link: PathBuf,
    daemon: std::process::Child,
}

impl Paired {
    fn new(fixture: &Fixture, predecessor: &Path) -> Self {
        let runner = ChildRunner::of(fixture);
        let link = fixture.root.join("paired-command");
        symlink(predecessor, &link).unwrap();
        let bin = fixture.root.join("bridge-bin");
        std::fs::create_dir(&bin).unwrap();
        let shim = bin.join("ssh");
        let home = runner.home.to_str().unwrap();
        let state = runner.home.join("os-state");
        std::fs::write(&shim, format!(
            "#!/bin/sh\nn=0\nwhile [ \"$1\" != -- ]; do [ \"$n\" -lt 64 ] || exit 2; n=$((n + 1)); shift; done\nshift\n[ \"$1\" = fixture ] || exit 2\nshift\n[ \"$#:$1\" != 1:true ] || exit 0\n[ \"$1\" = {} ] || exit 2\nexport HOME={} PLAK_HOME={} XDG_STATE_HOME={}\nexec \"$@\"\n",
            shell_words::quote(link.to_str().unwrap()), shell_words::quote(home),
            shell_words::quote(home), shell_words::quote(state.to_str().unwrap()),
        )).unwrap();
        std::fs::set_permissions(shim, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(fixture.home().join("host-id"), CHILD_RUNNER_HOST).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let daemon = runner.start_daemon_from(predecessor);
        Self {
            id: String::new(),
            path,
            link,
            daemon,
        }
    }

    fn pair(&mut self, fixture: &Fixture, predecessor: &Path) {
        run(fixture
            .base_command_from(predecessor)
            .env("PATH", &self.path)
            .args(["runner", "pair", "paired"]));
        let pin: Value = serde_json::from_slice(
            &std::fs::read(fixture.home().join("runners/paired/runner.json")).unwrap(),
        )
        .unwrap();
        pin["runner_id"].as_str().unwrap().clone_into(&mut self.id);
    }

    fn advance(&self, candidate: &Path) {
        let next = self.link.with_extension("next");
        symlink(candidate, &next).unwrap();
        std::fs::rename(next, &self.link).unwrap();
    }

    fn ready(&self, client: &mut Client) {
        use plakous_host_protocol::wire::RunnerStatus;
        let deadline = Instant::now() + WAIT;
        loop {
            let Outcome::Ok(Answer::Options { options }) =
                client.request(8, Op::SessionOptions { workspace: None })
            else {
                panic!("runner picker refused");
            };
            let row = options
                .runners
                .iter()
                .find(|row| row.runner.runner_id == self.id);
            if row.is_some_and(|row| row.status == RunnerStatus::Ready) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "paired runner not ready: {:?}",
                options.runners
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn configure(fixture: &Fixture, paired: &Paired) -> PathBuf {
    use crate::web_endpoint::{CSP, LOGIN, ORIGIN};
    let home = fixture.home();
    let gateway = fixture.gateway_socket();
    write_private(
        &home.join("gateway.toml"),
        &format!("socket = {:?}\n", gateway.to_str().unwrap()),
    );
    let server = PathBuf::from(std::env::var_os("PLAK_MCP_SERVER").expect("offline MCP server"));
    let mcp_log = fixture.root.join("mcp-list.log");
    let mcp_log = mcp_log.to_str().unwrap();
    let server = server.to_str().unwrap();
    fixture.write_config(&format!(
        "[root]\nagent = \"lead\"\n[orchestration]\nsubagents = true\n\
         [compaction]\nenabled = false\n[web]\nallow = [{LOGIN:?}]\norigin = {ORIGIN:?}\n\
         [runners.paired]\nssh = \"fixture\"\ncommand = {:?}\n\
         [mcp_servers.fake]\ntransport = \"stdio\"\ncommand = {server:?}\nargs = [\"--list-log\", {mcp_log:?}]\n",
        paired.link.to_str().unwrap(),
    ));
    let ui = fixture.root.join("ui");
    std::fs::create_dir(&ui).unwrap();
    std::fs::write(
        ui.join("index.html"),
        "<!doctype html><title>Offline upgrade</title>",
    )
    .unwrap();
    std::fs::write(ui.join("csp.json"), json!({"header": CSP}).to_string()).unwrap();
    fixture.root.join("w.sock")
}

fn seed_script(fixture: &Fixture, original: &Value) {
    let mut script = original.clone();
    let marker = fixture.root.join("job.marker");
    let marker = shell_words::quote(marker.to_str().unwrap());
    let job = format!(
        "marker={marker}; printf '%s %s\\n' \"$$\" \"$PPID\" >> \"$marker\"; \
         n=0; until [ -f \"${{marker%.marker}}.release\" ]; do [ \"$n\" -lt 3000 ] || exit 42; \
         n=$((n + 1)); sleep 0.05; done; printf 'retained output\\n'; exit 7"
    );
    script["steps"] = json!([
        {"output":{"text":HISTORY[0]}},
        {"output":{"text":HISTORY[1]}},
        {"output":{"tool":{"name":"process_session_start","arguments":{
            "command": job,
            "workdir":".","login":false,"tty":false,"keep_stdin_open":false,
            "yield_time_ms":0,"max_output_bytes":4096
        }}}, "finish":"tool_calls"},
        {"output":{"text":HISTORY[2]}},
        original["steps"][0].clone(),
        stream_step(fixture),
        {"output":{"text":RESUMED}},
    ]);
    fixture.write_script(&script);
}

fn stream_step(fixture: &Fixture) -> Value {
    json!({
        "stream": [{"text": STREAM}, {"gate": {"file": fixture.root.join("finish-stream")}}],
        "output": {"text": FINAL}
    })
}

fn start_gateway(fixture: &Fixture, image: &Path) -> GatewayProcess {
    GatewayProcess::start(
        &mut fixture.base_command_from(image),
        &fixture.gateway_socket(),
        &fixture.root.join("gateway.log"),
    )
}

fn gateway_check(fixture: &Fixture, image: &Path) -> String {
    let text = run(fixture.base_command_from(image).args(["gateway", "check"]));
    assert!(
        text.lines().any(|line| line.starts_with("account ")),
        "{text}"
    );
    text.lines()
        .find_map(|line| line.strip_prefix("gateway "))
        .and_then(|line| line.split_once(" protocol "))
        .unwrap()
        .0
        .to_owned()
}

fn start_local_runner(fixture: &Fixture, image: &Path) -> Child {
    let log = std::fs::File::create(fixture.root.join("runner.log")).unwrap();
    let mut runner = Child::spawn(
        fixture
            .base_command_from(image)
            .args(["runner", "serve", "--local"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log),
    );
    eventually("predecessor local runner did not listen", || {
        assert!(
            runner.try_wait().unwrap().is_none(),
            "{}",
            std::fs::read_to_string(fixture.root.join("runner.log")).unwrap()
        );
        std::fs::read_dir(fixture.home().join("host/runner"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| entry.path().join("run/r.sock").exists())
    });
    runner
}

fn start_host(
    fixture: &Fixture,
    image: &Path,
    paired: &Paired,
    socket: &Path,
    workspace: &Path,
) -> Host {
    let shell = fixture.root.join("login-shell");
    let calls = fixture.root.join("shell-invocations");
    let calls = shell_words::quote(calls.to_str().unwrap());
    std::fs::write(
        &shell,
        format!("#!/bin/sh\nprintf '%s\\n' \"$PPID\" >> {calls}\nprintf '%s' \"$PATH\"\n"),
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
    await_serving_host(spawn_host(
        fixture
            .base_command_from(image)
            .current_dir(workspace)
            .env("PATH", &paired.path)
            .env("SHELL", shell)
            .args([
                "host",
                "--web",
                &format!("unix:{}", socket.display()),
                "--ui",
            ])
            .arg(fixture.root.join("ui")),
    ))
    .unwrap()
}

struct Root {
    id: String,
    node: NodeAddress,
    locator: plakous::sessions::SessionLocator,
}

fn seed(
    fixture: &Fixture,
    client: &mut Client,
    workspace: &Path,
    predecessor: &Path,
    document: &Path,
) -> Root {
    let (session, node) = create_root(client, workspace);
    let attachment = client.attach(2, &node);
    let product = source::open(fixture);
    for (index, answer) in HISTORY.into_iter().enumerate() {
        client.prompt(3, attachment, &format!("fixture prompt {index}"), answer);
        if index == 1 {
            source::idle_locator(&product, &session, 2);
            run(fixture
                .base_command_from(predecessor)
                .current_dir(workspace)
                .args(["session", "export", &session, "--out"])
                .arg(document));
        }
    }
    Root {
        locator: source::idle_locator(&product, &session, HISTORY.len().try_into().unwrap()),
        id: session,
        node,
    }
}

fn create_root(client: &mut Client, workspace: &Path) -> (String, NodeAddress) {
    let created = client.request(1, new_session(workspace));
    let Outcome::Ok(Answer::Created { session, node, .. }) = created else {
        panic!("predecessor refused fixture session creation: {created:?}");
    };
    (session, node)
}

fn locator(fixture: &Fixture, session: &str) -> plakous::sessions::SessionLocator {
    SqliteSessionStore::new(PlakPaths::explicit(fixture.home()))
        .load(session)
        .unwrap()
        .plakous_locator
        .unwrap()
}

fn lifecycles(fixture: &Fixture, root: &Root) -> Vec<Value> {
    let paths = PlakPaths::explicit(fixture.home());
    let (open, mut terminals) =
        inspect_root_lifecycles(&paths.runtime_database(), root.locator).unwrap();
    assert!(open.is_none(), "root still has open turn {open:?}");
    // Drain's internal cancellation is a separate command for the same turn,
    // not a second prompt admission or terminal publication.
    terminals.retain(|row| row["command"].as_str().unwrap().starts_with("plak.queue."));
    terminals
}

fn assert_terminals(ledger: &[Value], expected: usize) {
    assert_eq!(ledger.len(), expected, "{ledger:?}");
    assert_eq!(
        ledger
            .iter()
            .map(|row| row["turn"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        expected
    );
    assert!(
        ledger
            .iter()
            .all(|row| matches!(row["state"].as_str(), Some("completed" | "finalized")))
    );
}

fn check_job(fixture: &Fixture, daemon: i32) -> (i32, i32) {
    let marker = fixture.root.join("job.marker");
    eventually("background job did not start", || {
        std::fs::read_to_string(&marker).is_ok_and(|row| row.ends_with('\n'))
    });
    let value = std::fs::read_to_string(marker).unwrap();
    assert_eq!(value.lines().count(), 1, "{value}");
    let (pid, owner) = value.trim().split_once(' ').unwrap();
    let pid = pid.parse().unwrap();
    assert_eq!(owner.parse::<i32>().unwrap(), daemon, "job changed runner");
    assert!(runner::alive(pid), "job {pid} exited before its release");
    (pid, daemon)
}

fn assert_retained_output(fixture: &Fixture, root: &Root) {
    let db = PlakPaths::explicit(fixture.home()).runtime_database();
    eventually(
        "the original runner's job notice did not reach the new host",
        || inspect_process_notices(&db, root.locator).unwrap() == [1],
    );
    let checkpoint = inspect_branch_checkpoint(&db, root.locator).unwrap();
    let notices = checkpoint
        .manifest()
        .canonical()
        .history()
        .segments()
        .iter()
        .flat_map(plakous::model::HistorySegment::items)
        .flat_map(plakous::model::ConversationItem::blocks)
        .filter_map(|block| match block {
            plakous::model::ContentBlock::Text(text)
                if text.text().contains("<process_notice ") =>
            {
                Some(text.text())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].contains("retained output") && notices[0].contains("exit_code=\"7\""),
        "{notices:?}"
    );
}
