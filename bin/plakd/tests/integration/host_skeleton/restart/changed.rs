//! A restart to a candidate with a changed runner triple: the candidate
//! speaks only its own native triple; both original daemons keep their
//! process groups and answer through the retention socket.

use super::*;
use std::path::Path;

use plakous_offline_crash_harness::ledger_signature::inspect_process_notices;

mod children;
mod durability;

const REVISION: &str = "candidate-runner-revision";

fn candidate_environment(image: &Path) -> [(&'static str, String); 2] {
    [
        (
            "PLAK_TEST_HOST_UPGRADE_RUNNER_IMAGE",
            image.display().to_string(),
        ),
        ("PLAK_TEST_HOST_UPGRADE_RUNNER_REVISION", REVISION.into()),
    ]
}

#[test]
fn paired_check_reopen_refuses_without_starting_the_candidate_daemon() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let paired = Paired::new(&fixture);
    fixture.write_script(&json!({
        "version": 1, "repeat_last_step": true, "steps": [{"output": {"text": ANSWER}}]
    }));
    let mut environment = candidate_environment(&image).to_vec();
    environment.push(("PATH", paired.path.clone()));
    let mut host = start_env(&fixture, &[], &environment);
    let mut client = Client::connect(&host);
    runner::remote_ready(&mut client, &paired.id);
    let mut request = new_session(&fixture.workspace());
    if let Op::NewSession { runner, .. } = &mut request {
        *runner = plakous_host_protocol::wire::RunnerLabel {
            name: "remote".into(),
            runner_id: paired.id.clone(),
        };
    }
    let Outcome::Ok(Answer::Created { session, node, .. }) = client.request(1, request) else {
        panic!("paired session was not created");
    };
    let attachment = client.attach(2, &node);
    client.prompt(3, attachment, "save a paired session", ANSWER);
    profiles::wait_idle(&mut client, &session);
    drop(client);
    stop(&mut host);

    let builds = paired.runner.home.join("runner/builds");
    let inventory = || {
        let mut names: Vec<_> = std::fs::read_dir(&builds)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort_unstable();
        names
    };
    let before = inventory();
    assert_eq!(before.len(), 1);
    paired.advance(&image);
    let output = image_command(&fixture, &image)
        .envs(environment)
        .args(["session", "check-reopen", &session])
        .output()
        .unwrap();
    assert_eq!(inventory(), before, "the probe started a new runner daemon");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("check-reopen: {error}: {output:?}"));
    assert_eq!(report["verdict"], "refused", "{report}");
    assert!(
        report["reason"]
            .as_str()
            .unwrap()
            .contains("check_reopen_runner_qualification"),
        "{report}"
    );
}

#[test]
fn local_and_paired_background_jobs_survive_a_changed_triple_restart() {
    let Some(fixture) = fixture() else { return };
    let _cleanup = HomeHost(fixture.home());
    let image = replacement_image(&fixture).canonicalize().unwrap();
    let paired = Paired::new(&fixture);
    source_script(&fixture);
    let mut environment = candidate_environment(&image).to_vec();
    environment.push(("PATH", paired.path.clone()));
    let mut host = start_env(&fixture, &[], &environment);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    runner::remote_ready(&mut client, &paired.id);
    let local = start_root(&fixture, &mut client, "local", None, 10);
    let remote = start_root(&fixture, &mut client, "paired", Some(&paired.id), 20);
    for root in [&local, &remote] {
        assert_eq!(root_record(&fixture, &root.session).1, 0);
        assert_eq!(root.background.1, root.foreground.1);
        assert!(runner::alive(root.background.0) && runner::alive(root.foreground.0));
    }
    assert_eq!(Some(remote.foreground.1), paired.runner.pid());
    assert_ne!(local.foreground.1, remote.foreground.1);

    paired.advance(&image);
    destination_script(&fixture);
    drop(client);
    let mut host = restart_to(&fixture, &mut host, &image, &environment);
    runner::wait_local_runner_ready(&host);
    let mut client = Client::connect(&host);
    runner::remote_ready(&mut client, &paired.id);
    for root in [&local, &remote] {
        // The promoted background job outlives the planned stop on its
        // original daemon; the unpromoted foreground call ends with the stop
        // and is not replayed.
        assert!(
            runner::alive(root.background.0),
            "{}: background job",
            root.name
        );
        assert_eq!(root.background, marked(&fixture, &root.name, "bg"));
        wait_exit(root.foreground.0);
        assert_eq!(root.foreground, marked(&fixture, &root.name, "fg"));
        std::fs::write(fixture.root.join(format!("{}-bg-finish", root.name)), "").unwrap();
    }
    for root in [&local, &remote] {
        wait_exit(root.background.0);
    }
    release_continuation(&fixture);
    for root in [&local, &remote] {
        let (_, fresh_daemon) = marked(&fixture, &root.name, "new");
        assert_ne!(fresh_daemon, root.foreground.1);
        profiles::wait_idle(&mut client, &root.session);
        assert_eq!(root_record(&fixture, &root.session).1, 1);
    }
    paired.wait_original_exit();
    drop(client);
    stop(&mut host);
    let paths = plakd::home::paths::PlakPaths::explicit(fixture.home());
    for root in [&local, &remote] {
        let (head, _) = identity(&fixture, &root.session);
        let delivered = inspect_process_notices(&paths.runtime_database(), head).unwrap();
        assert_eq!(
            delivered.iter().filter(|job| **job == 1).count(),
            1,
            "{}: the retained background job's notice is delivered once",
            root.name
        );
    }
}

struct Paired {
    runner: ChildRunner,
    path: String,
    id: String,
    link: PathBuf,
}

impl Paired {
    fn new(fixture: &Fixture) -> Self {
        let (runner, path, id) = runner::remote_fixture(fixture);
        let link = fixture.root.join("paired-command");
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_plakd"), &link).unwrap();
        let config = fixture.home().join("config.toml");
        let text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            config,
            text.replace(env!("CARGO_BIN_EXE_plakd"), link.to_str().unwrap()),
        )
        .unwrap();
        Self {
            runner,
            path,
            id,
            link,
        }
    }

    fn advance(&self, image: &Path) {
        let next = self.link.with_extension("next");
        std::os::unix::fs::symlink(image, &next).unwrap();
        std::fs::rename(next, &self.link).unwrap();
    }

    fn wait_original_exit(&self) {
        let deadline = Instant::now() + WAIT;
        while self.runner.pid().is_some() {
            assert!(
                Instant::now() < deadline,
                "original paired daemon did not idle-exit"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
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
    node: NodeAddress,
    session: String,
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
        node,
        session,
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
