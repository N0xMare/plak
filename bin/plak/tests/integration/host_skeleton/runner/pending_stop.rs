//! A saved remote placement is cancellable before its deferred first bind.

use super::foreground::Paused;
use super::*;

fn remote(fixture: &Fixture) -> (ChildRunner, String, String) {
    use std::fmt::Write as _;
    use std::io::Read as _;
    use std::os::unix::fs::PermissionsExt as _;
    let script = job_script(fixture, false);
    fixture.write_script(&script);
    let runner = ChildRunner::of(fixture);
    let selected: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.script()).unwrap()).unwrap();
    let home = selected["child_runner"]["home"].as_str().unwrap();
    fixture.write_script(&script);
    runner.start();
    let bin = fixture.root.join("ssh-bin");
    std::fs::create_dir(&bin).unwrap();
    let ssh = bin.join("ssh");
    std::fs::write(&ssh, format!("#!/bin/sh\nn=0\nwhile [ \"$1\" != -- ]; do [ \"$n\" -lt 64 ] || exit 1; n=$((n + 1)); shift; done\nshift\nshift\nif [ \"$3\" = connect ]; then echo $$ >> '{}'; fi\nexport PLAK_HOME='{home}'\nexec \"$@\"\n", fixture.root.join("ssh-bridges").display())).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let config = fixture.home().join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    write!(
        text,
        "\n[runners.remote]\nssh = \"fixture\"\ncommand = {:?}\n",
        env!("CARGO_BIN_EXE_plak")
    )
    .unwrap();
    std::fs::write(config, text).unwrap();
    let mut bridge = fixture
        .command()
        .args(["runner", "connect", "--stdio"])
        .env("PLAK_HOME", &runner.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = bridge.stdout.take().unwrap();
    let mut prefix = [0; 4];
    output.read_exact(&mut prefix).unwrap();
    let length = usize::try_from(u32::from_be_bytes(prefix)).unwrap();
    assert!(length <= plakous::runner::protocol::frame::MAX_FRAME_BYTES);
    let mut body = vec![0; length];
    output.read_exact(&mut body).unwrap();
    let plakous::runner::protocol::RunnerFrame::Hello(hello) =
        plakous::runner::protocol::frame::decode(&body).unwrap()
    else {
        panic!("no runner hello")
    };
    bridge.kill().unwrap();
    bridge.wait().unwrap();
    install_fixture_pairing(fixture, &hello);
    (runner, path, hello.runner_id)
}

fn install_fixture_pairing(fixture: &Fixture, hello: &plakous::runner::protocol::Hello) {
    // Arrange an already-paired home from the actual daemon's identity. This
    // recovery shape also runs on ordinary unlabelled Cargo test builds, whose
    // production pairing command correctly refuses new remote pairings.
    std::fs::write(fixture.home().join("host-id"), CHILD_RUNNER_HOST).unwrap();
    let directory = fixture.home().join("runners/remote");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("runner.json"),
        serde_json::to_vec(&json!({
            "runner_id": hello.runner_id,
            "source_revision": hello.version.source_revision,
            "flavor": hello.version.flavor,
            "binary_id": hello.binary_id,
            "machine_id": hello.machine_id.as_ref().unwrap(),
            "roots": [fixture.workspace().canonicalize().unwrap().to_str().unwrap()]
        }))
        .unwrap(),
    )
    .unwrap();
}

fn start_remote_job(fixture: &Fixture, path: &str, id: &str) -> (Host, String) {
    let host = start_env(fixture, &[], &[("PATH", path.into())]);
    let mut client = Client::connect(&host);
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
    client.prompt(3, attachment, "remote promoted job", ANSWER);
    job(fixture);
    (host, session)
}

fn ready(client: &mut Client, id: &str) {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let Outcome::Ok(Answer::Options { options }) =
            client.request(8, Op::SessionOptions { workspace: None })
        else {
            panic!("options refused")
        };
        if options.runners.iter().any(|row| {
            row.runner.runner_id == id
                && row.status == plakous_host_protocol::wire::RunnerStatus::Ready
        }) {
            return;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn runner_deferred_bind_finishes_pending_stop_without_redispatch() {
    use std::os::unix::process::ExitStatusExt as _;
    let Some(fixture) = fixture() else { return };
    let (runner, path, id) = remote(&fixture);
    let (mut host, session) = start_remote_job(&fixture, &path, &id);
    let (pid, daemon) = job(&fixture);
    kill(&mut host);
    let mut host = start_env(&fixture, &[], &[("PATH", path.clone())]);
    let mut client = Client::connect(&host);
    ready(&mut client, &id);
    let mut asleep = Paused::new(daemon);
    let bridge: i32 = std::fs::read_to_string(fixture.root.join("ssh-bridges"))
        .unwrap()
        .lines()
        .last()
        .unwrap()
        .parse()
        .unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(bridge),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    assert_eq!(
        client.request(
            10,
            Op::StopSession {
                session: session.clone()
            }
        ),
        Outcome::Ok(Answer::Stopped {
            runner_pending: true
        })
    );
    assert_eq!(
        root_record(&fixture, &session).0.last(),
        Some(&RootInterruptCause::Stopped)
    );
    assert!(alive(pid), "pending cancellation claimed process death");
    kill(&mut host);
    // First later bind cuts each sweep/recover/recorded-stop boundary. The
    // durable intent predates all of them and never starts another job.
    for phase in [
        "lease",
        "transfer",
        "terminals",
        "before-stop",
        "after-stop",
    ] {
        let env = [
            ("PATH", path.clone()),
            ("PLAK_TEST_HOST_READOPT_CUT", phase.into()),
        ];
        let mut host = if phase == "lease" {
            start_env(&fixture, &[], &env)
        } else {
            let (child, lines) = spawn_env(&fixture, &[], &env);
            Host {
                child: Some(child),
                socket: PathBuf::new(),
                lines,
            }
        };
        if phase == "lease" {
            assert_eq!(Client::connect(&host).rows(20).len(), 1);
            asleep.resume();
        }
        let status = cut_exit(host.child.as_mut().unwrap());
        host.child.take();
        assert_eq!(status.signal(), Some(9), "missing {phase} cut");
        job(&fixture);
    }
    stopped(pid);
    runner.kill();
    runner.start();
    verify_repeated_stop(&fixture, &path, &session);
}

fn verify_repeated_stop(fixture: &Fixture, path: &str, session: &str) {
    for _ in 0..2 {
        let mut host = start_env(fixture, &[], &[("PATH", path.into())]);
        let mut client = Client::connect(&host);
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            let outcome = client.request(
                30,
                Op::StopSession {
                    session: session.into(),
                },
            );
            if outcome
                == Outcome::Ok(Answer::Stopped {
                    runner_pending: false,
                })
            {
                break;
            }
            assert_eq!(
                outcome,
                Outcome::Ok(Answer::Stopped {
                    runner_pending: true
                })
            );
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            root_record(fixture, session)
                .0
                .iter()
                .filter(|cause| **cause == RootInterruptCause::Stopped)
                .count(),
            1
        );
        job(fixture);
        stop(&mut host);
    }
}
