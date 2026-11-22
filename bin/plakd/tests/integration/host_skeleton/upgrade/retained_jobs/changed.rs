//! The candidate speaks only its own native triple; both original daemons
//! keep their process groups and answer through the retention socket.

use super::*;
use std::path::Path;

use plakous_offline_crash_harness::ledger_signature::inspect_process_notices;

mod durability;

const REVISION: &str = "candidate-runner-revision";

pub(in super::super) fn candidate_environment(image: &Path) -> [(&'static str, String); 2] {
    [
        (
            "PLAK_TEST_HOST_UPGRADE_RUNNER_IMAGE",
            image.display().to_string(),
        ),
        ("PLAK_TEST_HOST_UPGRADE_RUNNER_REVISION", REVISION.into()),
    ]
}

pub(in super::super) fn candidate_upgrade(
    fixture: &Fixture,
    image: &Path,
    environment: &[(&str, String)],
) -> std::process::Command {
    let probe = image_command(fixture, image)
        .envs(environment.iter().cloned())
        .args(["version", "--host-upgrade-probe"])
        .output()
        .unwrap();
    assert!(probe.status.success(), "{probe:?}");
    let facts: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    assert_eq!(facts["runner_version"]["source_revision"], REVISION);
    let mut request = image_command(fixture, image);
    request.envs(environment.iter().cloned()).args([
        "host",
        "upgrade",
        "--expect-binary",
        facts["binary_id"].as_str().unwrap(),
        "--drain",
        "0",
    ]);
    request
}

#[test]
fn local_and_paired_foreground_and_background_results_survive_changed_triple() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let (paired, path, paired_id) = runner::remote_fixture(&fixture);
    let link = fixture.root.join("paired-command");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_plakd"), &link).unwrap();
    let config = fixture.home().join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(
        &config,
        text.replace(env!("CARGO_BIN_EXE_plakd"), link.to_str().unwrap()),
    )
    .unwrap();
    source_script(&fixture);
    let mut environment = candidate_environment(&image).to_vec();
    environment.push(("PATH", path));
    let mut host = start_env(&fixture, &[], &environment);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    runner::remote_ready(&mut client, &paired_id);
    let local = start_root(&fixture, &mut client, "local", None, 10);
    let remote = start_root(&fixture, &mut client, "paired", Some(&paired_id), 20);
    for root in [&local, &remote] {
        assert_eq!(root_record(&fixture, &root.session).1, 0);
        assert_eq!(root.background.1, root.foreground.1);
        assert!(runner::alive(root.background.0) && runner::alive(root.foreground.0));
    }
    assert_eq!(Some(remote.foreground.1), paired.pid());
    assert_ne!(local.foreground.1, remote.foreground.1);

    let next_link = link.with_extension("next");
    std::os::unix::fs::symlink(&image, &next_link).unwrap();
    std::fs::rename(next_link, link).unwrap();
    destination_script(&fixture);
    let result = candidate_upgrade(&fixture, &image, &environment)
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    drop(client);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    runner::remote_ready(&mut client, &paired_id);
    for root in [&local, &remote] {
        assert!(runner::alive(root.background.0) && runner::alive(root.foreground.0));
        assert_eq!(root.foreground, marked(&fixture, &root.name, "fg"));
        assert_eq!(root.background, marked(&fixture, &root.name, "bg"));
        std::fs::write(fixture.root.join(format!("{}-bg-finish", root.name)), "").unwrap();
    }
    for root in [&local, &remote] {
        wait_exit(root.background.0);
        std::fs::write(fixture.root.join(format!("{}-fg-finish", root.name)), "").unwrap();
        wait_exit(root.foreground.0);
    }
    release_continuation(&fixture);
    for root in [&local, &remote] {
        notices(&fixture, &root.session, true, &root.name);
        assert_eq!(root_record(&fixture, &root.session).1, 1);
        let (_, fresh_daemon) = marked(&fixture, &root.name, "new");
        assert_ne!(fresh_daemon, root.foreground.1);
    }
    drop(client);
    stop(&mut host);
}

fn source_script(fixture: &Fixture) {
    let root = shell_words::quote(fixture.root.to_str().unwrap());
    let start = |kind, budget, code| {
        let command = format!(
            "base={root}/${{PWD##*/}}-{kind}; printf '%s %s\\n' \"$$\" \"$PPID\" >> \"$base.marker\"; n=0; until [ -f \"$base-finish\" ]; do [ \"$n\" -lt 10000 ] || exit 42; n=$((n + 1)); sleep 0.02; done; printf '{kind}-retained-output\\n'; exit {code}"
        );
        json!({"output":{"tool":{"name":"process_session_start","arguments":{
            "command":command, "workdir":".", "login":false, "tty":false, "keep_stdin_open":false,
            "yield_time_ms":budget, "max_output_bytes":8192
        }}},"finish":"tool_calls"})
    };
    fixture.write_script(&json!({
        "version":1, "process_tools":true,
        "steps":[start("bg", 0, 7), start("fg", 300_000, 9)]
    }));
}

fn destination_script(fixture: &Fixture) {
    let root = shell_words::quote(fixture.root.to_str().unwrap());
    let command = format!("printf '%s %s\\n' \"$$\" \"$PPID\" >> {root}/${{PWD##*/}}-new.marker");
    fixture.write_script(&json!({"version":1, "process_tools":true, "steps":[
        {"gate":{"file":fixture.root.join("resume-release")},
         "output":{"tool":{"name":"process_session_start","arguments":{
            "command": command, "workdir":".", "login":false, "tty":false, "keep_stdin_open":false,
            "yield_time_ms":30_000, "max_output_bytes":4096
        }}},"finish":"tool_calls"},
        {"output":{"text":ANSWER}},
        {"output":{"text":ANSWER}},
        {"output":{"text":ANSWER}}
    ]}));
}

fn release_continuation(fixture: &Fixture) {
    std::fs::write(fixture.root.join("resume-release"), "").unwrap();
}

struct Root {
    name: String,
    session: String,
    node: NodeAddress,
    background: (i32, i32),
    foreground: (i32, i32),
}

fn start_root(
    fixture: &Fixture,
    client: &mut Client,
    name: &str,
    runner: Option<&str>,
    id: u64,
) -> Root {
    let workspace = if runner.is_some() {
        fixture.workspace().join(name)
    } else {
        fixture.root.join(name)
    };
    std::fs::create_dir(&workspace).unwrap();
    let mut request = new_session_request(&workspace, None, &uuid::Uuid::new_v4().to_string());
    if let (Some(runner_id), Op::NewSession { runner: label, .. }) = (runner, &mut request) {
        *label = plakous_host_protocol::wire::RunnerLabel {
            name: "remote".into(),
            runner_id: runner_id.into(),
        };
    }
    let created = client.request(id, request);
    let Outcome::Ok(Answer::Created { session, node, .. }) = created else {
        panic!("creation refused for {name}: {created:?}");
    };
    let attachment = client.attach(id + 1, &node);
    submit(client, id + 2, attachment);
    Root {
        name: name.into(),
        session,
        node,
        background: marked(fixture, name, "bg"),
        foreground: marked(fixture, name, "fg"),
    }
}

fn submit(client: &mut Client, id: u64, attachment: u64) {
    assert_eq!(
        client.request(
            id,
            Op::Intent {
                client_id: Some(uuid::Uuid::new_v4().to_string().parse().unwrap()),
                attachment,
                intent: SessionIntent::Prompt(Draft::from("keep both jobs alive")),
                blobs: Vec::new(),
            }
        ),
        Outcome::Ok(Answer::Accepted { duplicate: false })
    );
}

fn marked(fixture: &Fixture, name: &str, job: &str) -> (i32, i32) {
    let marker = fixture.root.join(format!("{name}-{job}.marker"));
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(text) = std::fs::read_to_string(&marker)
            && text.ends_with('\n')
        {
            assert_eq!(text.lines().count(), 1, "the command was replayed");
            let (pid, daemon) = text.trim().split_once(' ').unwrap();
            return (pid.parse().unwrap(), daemon.parse().unwrap());
        }
        assert!(
            Instant::now() < deadline,
            "process never reached {}",
            marker.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_exit(pid: i32) {
    let deadline = Instant::now() + WAIT;
    while runner::alive(pid) {
        assert!(Instant::now() < deadline, "job {pid} never exited");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn notices(fixture: &Fixture, session: &str, exited: bool, context: &str) {
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    let deadline = Instant::now() + WAIT;
    loop {
        let (head, _) = identity(fixture, &plakd::home::state::state_key(), session);
        let mut seen = inspect_process_notices(&paths.runtime_database(), head).unwrap();
        seen.sort_unstable();
        if seen == [1, 2] {
            let checkpoint =
                plakous_offline_crash_harness::ledger_signature::inspect_branch_checkpoint(
                    &paths.runtime_database(),
                    head,
                )
                .unwrap();
            let result = checkpoint
                .manifest()
                .canonical()
                .history()
                .segments()
                .iter()
                .flat_map(plakous::model::HistorySegment::items)
                .filter(|item| item.provenance() == plakous::model::Provenance::Instruction)
                .flat_map(plakous::model::ConversationItem::blocks)
                .filter_map(|block| match block {
                    plakous::model::ContentBlock::Text(text) => Some(text.text()),
                    _ => None,
                })
                .filter(|text| text.contains("<process_notice "))
                .collect::<Vec<_>>()
                .join("\n");
            if exited {
                assert!(
                    result.contains("exit_code=\"7\"") && result.contains("bg-retained-output"),
                    "{result}"
                );
                assert!(
                    result.contains("exit_code=\"9\"") && result.contains("fg-retained-output"),
                    "{result}"
                );
            } else {
                assert_eq!(
                    result.matches("status=\"outcome_unknown\"").count(),
                    2,
                    "{result}"
                );
            }
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{context}: job notices: {seen:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
