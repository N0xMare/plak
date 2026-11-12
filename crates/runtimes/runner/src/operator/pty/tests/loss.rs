//! Real escaped and orphaned processes: loss is not process-death evidence.

use super::*;

pub(super) struct Escape(Identity);

impl Escape {
    pub(super) fn from_output(output: &Mutex<Output>) -> Self {
        let bytes: Vec<_> = lock(output).bytes.iter().copied().collect();
        let text = String::from_utf8_lossy(&bytes);
        let pid: u32 = text
            .split("ESCAPED ")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        Self(Identity::capture(pid).unwrap())
    }
}

impl Drop for Escape {
    fn drop(&mut self) {
        if Identity::capture(self.0.pid)
            .is_ok_and(|row| row.started == self.0.started && row.session == self.0.session)
        {
            let pid = rustix::process::Pid::from_raw(i32::try_from(self.0.pid).unwrap()).unwrap();
            // Test teardown of a freshly checked individual, never a group or
            // recovery authority inferred from a vanished runner table.
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
}

#[test]
fn operator_pty_unknown_close_reproves_or_acknowledges_surviving_escape_without_kill() {
    for acknowledge_loss in [false, true] {
        let mut worker = worker();
        until_output(&mut worker, b"PTY-READY\n");
        worker.io_round(&mut VecDeque::from(b"D".to_vec())).unwrap();
        until_output(&mut worker, b"ESCAPED ");
        let escape = Escape::from_output(&worker.output);
        let escaped_pid = escape.0.pid;
        let (sender, receive) = sync_channel(3);
        let closing = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let survivors = Arc::new(Mutex::new(Vec::new()));
        let ended = result.clone();
        let close = closing.clone();
        let escaped = survivors.clone();
        let thread = std::thread::spawn(move || {
            worker.run(&receive, &close, &ended, &Mutex::new(None), &escaped);
        });
        // A complete observation sees the live child before its parent disappears.
        std::thread::sleep(Duration::from_millis(1200));
        closing.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(5);
        while lock(&result).is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(lock(&result).unwrap().0, TerminalStatus::TerminationUnknown);
        assert!(lock(&survivors).iter().any(|row| row.pid == escaped_pid));
        if acknowledge_loss {
            assert!(!Identity::capture(escaped_pid).unwrap().zombie);
        } else {
            drop(escape);
        }
        sender.send(Command::Reprove { acknowledge_loss }).unwrap();
        thread.join().unwrap();
        assert_eq!(
            lock(&result).unwrap().0,
            if acknowledge_loss {
                TerminalStatus::Lost
            } else {
                TerminalStatus::Exited
            }
        );
        if acknowledge_loss {
            assert!(lock(&survivors).iter().any(|row| row.pid == escaped_pid));
            assert!(
                !Identity::capture(escaped_pid).unwrap().zombie,
                "acknowledge loss must not kill escapees"
            );
        }
    }
}

#[test]
#[ignore = "fast double-fork escape subprocess fixture"]
#[expect(
    clippy::zombie_processes,
    reason = "deliberate reparented escape; the outer test owns identity-checked teardown"
)]
fn operator_fast_escape_broker() {
    rustix::process::setsid().unwrap();
    let child = std::process::Command::new("/bin/sleep")
        .arg("600")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    println!("ESCAPED {}", child.id());
}

#[test]
fn operator_pty_fast_double_fork_escape_is_not_a_descendant_containment_claim() {
    let mut worker = worker();
    until_output(&mut worker, b"PTY-READY\n");
    worker.io_round(&mut VecDeque::from(b"F".to_vec())).unwrap();
    until_output(&mut worker, b"ESCAPED ");
    let escape = Escape::from_output(&worker.output);
    let result = Mutex::new(None);
    let (sender, receive) = sync_channel(3);
    drop(sender);
    worker.run(
        &receive,
        &AtomicBool::new(true),
        &result,
        &Mutex::new(None),
        &Mutex::new(Vec::new()),
    );
    // A setsid child reparented before any observation is not discoverable from
    // the former SID. Exited proves that SID absent, not arbitrary containment.
    assert_eq!(lock(&result).unwrap().0, TerminalStatus::Exited);
    assert!(!Identity::capture(escape.0.pid).unwrap().zombie);
}

#[test]
#[ignore = "real orphaned shell runner-loss subprocess fixture"]
fn operator_orphaned_shell_runner() {
    let directory = tempfile::tempdir().unwrap();
    let mut launch =
        account_launch(directory.path().to_owned(), "/usr/bin:/bin".into(), None).unwrap();
    launch.shell = "/bin/sh".into();
    let mut worker = Worker::spawn(
        launch,
        TerminalDimensions::new(24, 80).unwrap(),
        Arc::default(),
    )
    .unwrap();
    worker
        .io_round(&mut VecDeque::from(
            b"trap '' HUP; printf 'ORPHAN-READY\\n'; n=0; while [ \"$n\" -lt 60 ]; do n=$((n + 1)); sleep 1; done\n".to_vec(),
        ))
        .unwrap();
    until_output(&mut worker, b"ORPHAN-READY\r\n");
    std::fs::write(
        std::env::var_os("PLAK_OPERATOR_LOSS_HANDSHAKE").unwrap(),
        worker.identity.as_ref().unwrap().pid.to_string(),
    )
    .unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn operator_runner_loss_with_a_real_orphaned_shell_reports_lost_not_dead() {
    let directory = tempfile::tempdir().unwrap();
    let handshake = directory.path().join("ready");
    let mut runner = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "operator::pty::tests::loss::operator_orphaned_shell_runner",
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
        assert!(Instant::now() < deadline);
        assert!(runner.try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = std::fs::read_to_string(&handshake)
        .unwrap()
        .parse()
        .unwrap();
    let escape = Escape(Identity::capture(pid).unwrap());
    runner.kill().unwrap();
    runner.wait().unwrap();
    assert!(
        !Identity::capture(pid).unwrap().zombie,
        "real HUP-ignoring shell survives its runner"
    );
    let service = OperatorTerminalService::new(
        "replacement".into(),
        Arc::new(|_| panic!("loss never authorizes spawn")),
        None,
    )
    .unwrap();
    let id = |s: &str| OperatorId::try_from(s.to_owned()).unwrap();
    assert_eq!(
        service.replay_open(
            "paired",
            "generation",
            OperatorNamespace {
                state_key: id("state"),
                principal: id("operator")
            },
            "digest",
            &TerminalKey {
                session: id("session"),
                node: id("node"),
                terminal_id: id("terminal")
            }
        ),
        OperatorAnswer::Refused {
            refusal: OperatorRefusal::TerminalLost
        }
    );
    assert!(!Identity::capture(escape.0.pid).unwrap().zombie);
}
