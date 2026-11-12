use super::super::{
    OperatorAnswer, OperatorId, OperatorNamespace, OperatorTerminalService, TerminalKey,
};
use super::*;

#[test]
#[ignore = "subprocess fixture, invoked by operator PTY tests"]
fn operator_pty_child() {
    let mut terminal = rustix::termios::tcgetattr(std::io::stdin()).unwrap();
    terminal.make_raw();
    rustix::termios::tcsetattr(
        std::io::stdin(),
        rustix::termios::OptionalActions::Now,
        &terminal,
    )
    .unwrap();
    let mut stdout = std::io::stdout();
    stdout.write_all(b"\x1b[?1049hPTY-READY\n").unwrap();
    stdout.flush().unwrap();
    let mut nested = Vec::new();
    loop {
        let mut op = [0; 1];
        std::io::stdin().read_exact(&mut op).unwrap();
        match op[0] {
            b'E' => {
                let mut bytes = [0; 4];
                std::io::stdin().read_exact(&mut bytes).unwrap();
                stdout.write_all(&bytes).unwrap();
            }
            b'S' => {
                let size = rustix::termios::tcgetwinsize(std::io::stdin()).unwrap();
                writeln!(stdout, "SIZE {} {}", size.ws_row, size.ws_col).unwrap();
            }
            b'Q' => return,
            b'R' => {
                std::io::stderr().write_all(b"STDERR\0\xff").unwrap();
            }
            b'D' => {
                let pair = native_pty_system()
                    .openpty(size(TerminalDimensions::new(24, 80).unwrap()))
                    .unwrap();
                let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
                command.args([
                    "--exact",
                    "operator::pty::tests::operator_pty_child",
                    "--ignored",
                    "--nocapture",
                ]);
                let child = pair.slave.spawn_command(command).unwrap();
                writeln!(stdout, "ESCAPED {}", child.process_id().unwrap()).unwrap();
                nested.push((pair, child));
            }
            _ => panic!("invalid fixture instruction"),
        }
        stdout.flush().unwrap();
    }
}

fn worker() -> Worker {
    let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "operator::pty::tests::operator_pty_child",
        "--ignored",
        "--nocapture",
    ]);
    command.env_clear();
    Worker::spawn_command(
        command,
        TerminalDimensions::new(24, 80).unwrap(),
        Arc::default(),
    )
    .unwrap()
}

fn until_output(worker: &mut Worker, needle: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let _ = worker.io_round(&mut VecDeque::new());
        if lock(&worker.output)
            .bytes
            .make_contiguous()
            .windows(needle.len())
            .any(|s| s == needle)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "fixture output never arrived: {:?}",
            lock(&worker.output).bytes
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn operator_pty_interactive_binary_echo_resize_and_two_absence_scans() {
    let mut worker = worker();
    until_output(&mut worker, b"PTY-READY\n");
    worker
        .io_round(&mut VecDeque::from(b"E\0\xff\x01\x02".to_vec()))
        .unwrap();
    until_output(&mut worker, &[0, 255, 1, 2]);
    worker.io_round(&mut VecDeque::from(b"R".to_vec())).unwrap();
    until_output(&mut worker, b"STDERR\0\xff");
    worker
        .master
        .as_ref()
        .unwrap()
        .resize(size(TerminalDimensions::new(41, 132).unwrap()))
        .unwrap();
    worker.io_round(&mut VecDeque::from(b"S".to_vec())).unwrap();
    until_output(&mut worker, b"SIZE 41 132\n");
    let identity = worker.identity.clone().unwrap();
    worker.io.take();
    worker.master.take();
    let mut cleanup = SessionCleanup::new(identity.clone());
    assert!(cleanup.finish(|| worker.child.try_wait().ok().flatten().is_some()));
    worker.cleaned = true;
    assert!(!scan().unwrap().iter().any(|r| r.pid == identity.pid));
}

#[test]
fn operator_pty_close_kills_background_stopped_and_foreground_job_groups() {
    let directory = tempfile::tempdir().unwrap();
    let mut launch =
        account_launch(directory.path().to_path_buf(), "/usr/bin:/bin".into(), None).unwrap();
    launch.shell = "/bin/sh".into();
    launch.home = directory.path().to_path_buf();
    let mut worker = Worker::spawn(
        launch,
        TerminalDimensions::new(24, 80).unwrap(),
        Arc::default(),
    )
    .unwrap();
    let executable = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .replace('\'', "'\\''");
    let input = format!(
        "trap '' HUP TERM; sleep 600 & sleep 600 & kill -STOP $!; echo GROUPS-READY; '{executable}' --exact operator::pty::tests::operator_pty_child --ignored --nocapture\n"
    );
    worker
        .io_round(&mut VecDeque::from(input.into_bytes()))
        .unwrap();
    until_output(&mut worker, b"GROUPS-READY\r\n");
    until_output(&mut worker, b"\x1b[?1049hPTY-READY\n");
    let identity = worker.identity.clone().unwrap();
    let rows = scan().unwrap();
    let groups: std::collections::BTreeSet<_> = rows
        .iter()
        .filter(|r| r.session == identity.session)
        .map(|r| r.group)
        .collect();
    assert!(
        groups.len() >= 4,
        "shell, background, stopped and full-screen job groups must be distinct"
    );
    let mut cleanup = SessionCleanup::new(identity.clone());
    cleanup.observe(&rows);
    worker.io.take();
    worker.master.take();
    assert!(cleanup.finish(|| worker.child.try_wait().ok().flatten().is_some()));
    worker.cleaned = true;
    assert!(!scan().unwrap().iter().any(|r| r.pid == identity.pid));
}

#[test]
fn operator_pty_setsid_escape_is_termination_unknown_even_after_master_hangup() {
    let mut worker = worker();
    until_output(&mut worker, b"PTY-READY\n");
    worker.io_round(&mut VecDeque::from(b"D".to_vec())).unwrap();
    until_output(&mut worker, b"ESCAPED ");
    let mut cleanup = SessionCleanup::new(worker.identity.clone().unwrap());
    cleanup.observe(&scan().unwrap());
    assert!(
        cleanup.uncertain,
        "different-session descendant is not cleanup proof"
    );
    worker.io.take();
    worker.master.take();
    assert!(!cleanup.finish(|| worker.child.try_wait().ok().flatten().is_some()));
    worker.cleaned = true;
}

#[test]
fn operator_pty_namespace_expiry_is_enforced_without_a_host_or_poll() {
    let directory = tempfile::tempdir().unwrap();
    let clock = Arc::new(Mutex::new(Instant::now()));
    let now = Arc::clone(&clock);
    let service = OperatorTerminalService::new(
        "instance".into(),
        Arc::new(|cwd| {
            let mut launch = account_launch(cwd.to_path_buf(), "/usr/bin:/bin".into(), None)?;
            launch.shell = "/bin/sh".into();
            launch.home = cwd.to_path_buf();
            Ok(launch)
        }),
        Some(Arc::new(move || *lock(&now))),
    )
    .unwrap();
    let id = |s: &str| OperatorId::try_from(s.to_owned()).unwrap();
    let namespace = OperatorNamespace {
        state_key: id("state"),
        principal: id("operator"),
    };
    let key = TerminalKey {
        session: id("session"),
        node: id("node"),
        terminal_id: id("terminal"),
    };
    let renewed = service.execute(
        "paired",
        "generation",
        1,
        namespace.clone(),
        "renew",
        super::super::OperatorOperation::Renew,
    );
    assert!(matches!(renewed, OperatorAnswer::Table { .. }));
    let opened = service.execute(
        "paired",
        "generation",
        1,
        namespace,
        "open",
        super::super::OperatorOperation::Open {
            key,
            workspace: directory.path().to_string_lossy().into_owned(),
            dimensions: TerminalDimensions::new(24, 80).unwrap(),
        },
    );
    assert!(
        matches!(opened,OperatorAnswer::Terminal {terminal,..} if terminal.status==TerminalStatus::Running)
    );
    *lock(&clock) += Duration::from_secs(90);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let done = lock(&service.inner.table)
            .namespaces
            .values()
            .flat_map(|ns| ns.terminals.values())
            .all(|terminal| terminal.status == TerminalStatus::Exited);
        if done {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "namespace expiry required a host poll"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "runner-loss subprocess fixture"]
fn operator_runner_loss_child() {
    let mut worker = worker();
    until_output(&mut worker, b"PTY-READY\n");
    let pid = worker.identity.as_ref().unwrap().pid;
    std::fs::write(
        std::env::var_os("PLAK_OPERATOR_LOSS_HANDSHAKE").unwrap(),
        pid.to_string(),
    )
    .unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn operator_runner_loss_sigkill_does_not_recover_or_recreate_shell() {
    let directory = tempfile::tempdir().unwrap();
    let handshake = directory.path().join("ready");
    let mut runner = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "operator::pty::tests::operator_runner_loss_child",
            "--ignored",
            "--nocapture",
        ])
        .env("PLAK_OPERATOR_LOSS_HANDSHAKE", &handshake)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handshake.exists() {
        assert!(Instant::now() < deadline, "runner fixture did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = std::fs::read_to_string(&handshake)
        .unwrap()
        .parse::<u32>()
        .unwrap();
    runner.kill().unwrap();
    runner.wait().unwrap();
    // The new runner has no process authority from a former instance's bare PID.
    // A normal-HUP deterministic child should die when the killed runner loses
    // its master; this observation is test teardown, not recovery authority.
    while scan()
        .unwrap()
        .iter()
        .any(|row| row.pid == pid && !row.zombie)
    {
        assert!(
            Instant::now() < deadline,
            "normal-HUP fixture survived master loss"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let service = OperatorTerminalService::new(
        "replacement".into(),
        Arc::new(|_| panic!("restart must never spawn")),
        None,
    )
    .unwrap();
    let id = |s: &str| OperatorId::try_from(s.to_owned()).unwrap();
    let namespace = OperatorNamespace {
        state_key: id("state"),
        principal: id("operator"),
    };
    let key = TerminalKey {
        session: id("session"),
        node: id("node"),
        terminal_id: id("terminal"),
    };
    assert_eq!(
        service.replay_open("paired", "generation", namespace, "digest", &key),
        OperatorAnswer::Refused {
            refusal: OperatorRefusal::TerminalLost
        }
    );
}
