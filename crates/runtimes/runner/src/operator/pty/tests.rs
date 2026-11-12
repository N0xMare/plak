use super::super::{
    OperatorAnswer, OperatorId, OperatorNamespace, OperatorTerminalService, TerminalKey,
};
use super::*;
mod loss;

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
                let mut command = CommandBuilder::new("/bin/sh");
                command.args(["-c", "trap '' HUP TERM; exec /bin/sleep 600"]);
                let child = pair.slave.spawn_command(command).unwrap();
                writeln!(stdout, "ESCAPED {}", child.process_id().unwrap()).unwrap();
                nested.push((pair, child));
            }
            b'F' => {
                let escaped = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "operator::pty::tests::loss::operator_fast_escape_broker",
                        "--ignored",
                        "--nocapture",
                    ])
                    .output()
                    .unwrap();
                stdout.write_all(&escaped.stdout).unwrap();
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

#[test]
fn operator_pty_platform_scan_distinguishes_runner_and_child_sessions() {
    probe_sessions().unwrap();
    let worker = worker();
    let rows = scan().unwrap();
    let own = rows
        .iter()
        .find(|row| row.pid == std::process::id())
        .unwrap();
    let child = worker.identity.as_ref().unwrap();
    assert_eq!(
        own.session,
        rustix::process::getsid(None)
            .unwrap()
            .as_raw_nonzero()
            .get()
            .to_string()
    );
    assert_ne!(own.session, child.session);
    assert_eq!(child.session, child.pid.to_string());
}

#[test]
fn operator_pty_env_has_exact_curated_keys_without_inherited_secrets() {
    let directory = tempfile::tempdir().unwrap();
    for ceiling in [false, true] {
        let mut launch =
            account_launch(directory.path().to_owned(), "/usr/bin:/bin".into(), None).unwrap();
        launch.discovery_ceiling = ceiling.then(|| directory.path().to_owned());
        let mut command = CommandBuilder::new("/usr/bin/env");
        curate(&mut command, launch);
        let mut worker = Worker::spawn_command(
            command,
            TerminalDimensions::new(24, 80).unwrap(),
            Arc::default(),
        )
        .unwrap();
        until_output(&mut worker, b"LANG=C.UTF-8");
        let bytes: Vec<_> = lock(&worker.output).bytes.iter().copied().collect();
        let text = String::from_utf8(bytes).unwrap();
        let keys: std::collections::BTreeSet<_> = text
            .lines()
            .map(|line| line.split_once('=').unwrap().0)
            .collect();
        let mut expected = std::collections::BTreeSet::from([
            "PATH", "HOME", "SHELL", "USER", "LOGNAME", "TERM", "LANG",
        ]);
        if ceiling {
            expected.insert("GIT_CEILING_DIRECTORIES");
        }
        assert_eq!(keys, expected);
    }
}

#[test]
fn operator_pty_passwd_shell_precedes_fallback_and_invalid_fallback_refuses() {
    let cwd = PathBuf::from("/workspace");
    let fallback = OperatorAccountFallback {
        shell: "/bin/sh".into(),
        home: "/home".into(),
        user: "fallback".into(),
    };
    let launch = launch_with_account(
        cwd.clone(),
        "/bin".into(),
        Some(("/bin/sh".into(), "/passwd-home".into(), "passwd".into())),
        Some(fallback.clone()),
    )
    .unwrap();
    assert_eq!(launch.user, "passwd");
    assert_eq!(launch.home, PathBuf::from("/passwd-home"));
    assert_eq!(
        launch_with_account(cwd.clone(), "/bin".into(), None, Some(fallback.clone()))
            .unwrap()
            .user,
        "fallback"
    );
    assert!(launch_with_account(cwd.clone(), "/bin".into(), None, None).is_err());
    for shell in ["relative", "/no-such-shell"] {
        let mut invalid = fallback.clone();
        invalid.shell = shell.into();
        assert!(launch_with_account(cwd.clone(), "/bin".into(), None, Some(invalid)).is_err());
    }
    let system = account_launch(cwd, "/bin".into(), Some(fallback)).unwrap();
    let passwd = nix::unistd::User::from_uid(nix::unistd::getuid())
        .unwrap()
        .unwrap();
    assert_eq!(system.shell, passwd.shell);
}

#[test]
fn operator_pty_transient_observation_failure_does_not_close_live_shell() {
    let mut worker = worker();
    until_output(&mut worker, b"PTY-READY\n");
    worker.observation_failures = 2;
    let (send, receive) = sync_channel(3);
    let close = Arc::new(AtomicBool::new(false));
    let result = Arc::new(Mutex::new(None));
    let ending = close.clone();
    let answer = result.clone();
    let output = worker.output.clone();
    let thread = std::thread::spawn(move || {
        worker.run(
            &receive,
            &ending,
            &answer,
            &Mutex::new(None),
            &Mutex::new(Vec::new()),
        );
    });
    std::thread::sleep(Duration::from_millis(2300));
    assert!(
        lock(&result).is_none(),
        "a failed observation is not a cleanup request"
    );
    send.send(Command::Input(b"EOKAY".to_vec())).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !lock(&output)
        .bytes
        .make_contiguous()
        .windows(4)
        .any(|bytes| bytes == b"OKAY")
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    close.store(true, Ordering::Release);
    thread.join().unwrap();
    assert_eq!(lock(&result).unwrap().0, TerminalStatus::Exited);
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
    let (sender, receive) = sync_channel(3);
    drop(sender);
    let result = Mutex::new(None);
    worker.run(
        &receive,
        &AtomicBool::new(true),
        &result,
        &Mutex::new(None),
        &Mutex::new(Vec::new()),
    );
    assert_eq!(
        lock(&result).unwrap().0,
        TerminalStatus::Exited,
        "closing flag, master hangup and whole-session cleanup must join"
    );
    assert!(!scan().unwrap().iter().any(|r| r.pid == identity.pid));
}

#[test]
fn operator_pty_setsid_escape_is_termination_unknown_even_after_master_hangup() {
    let mut worker = worker();
    until_output(&mut worker, b"PTY-READY\n");
    worker.io_round(&mut VecDeque::from(b"D".to_vec())).unwrap();
    until_output(&mut worker, b"ESCAPED ");
    let _escape = loss::Escape::from_output(&worker.output);
    let mut cleanup = SessionCleanup::new(worker.identity.clone().unwrap());
    cleanup.observe(&scan().unwrap());
    assert!(
        !cleanup.escaped.is_empty(),
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
