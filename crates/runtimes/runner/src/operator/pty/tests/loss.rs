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

    fn current(&self) -> Option<Identity> {
        Identity::capture_fixture_pid(self.0.pid)
            .ok()
            .filter(|row| row.started == self.0.started && row.session == self.0.session)
    }
}

impl Drop for Escape {
    fn drop(&mut self) {
        if self.current().is_some() {
            let pid = rustix::process::Pid::from_raw(i32::try_from(self.0.pid).unwrap()).unwrap();
            // Test teardown of a freshly checked individual, never a group or
            // recovery authority inferred from a vanished runner table.
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
}

#[test]
fn operator_pty_kills_a_stubborn_member_after_the_leader_exits_on_hangup() {
    let mut command = CommandBuilder::new("/bin/sh");
    command.args([
        "-c",
        r#"/bin/sh -c 'trap "" HUP TERM; printf "MEMBER %s\n" "$$"; exec /bin/sleep 600 </dev/null >/dev/null 2>&1' &
read -r input"#,
    ]);
    let mut worker = Worker::spawn_command(
        command,
        TerminalDimensions::new(24, 80).unwrap(),
        Arc::default(),
    )
    .unwrap();
    let member = Escape(Identity::capture(read_member_pid(&mut worker)).unwrap());
    assert_eq!(
        member.0.session,
        worker.identity.as_ref().unwrap().session,
        "the stubborn child must be in the operator session"
    );
    worker.io.take();
    worker.master.take();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !worker.leader_exited() {
        assert!(Instant::now() < deadline, "hangup did not end the leader");
        std::thread::yield_now();
    }
    assert!(
        member.current().is_some_and(|row| !row.zombie),
        "the member must still be live after the hangup"
    );

    let (sender, receive) = sync_channel(1);
    drop(sender);
    let result = Mutex::new(None);
    worker.run(
        &receive,
        &AtomicBool::new(true),
        &result,
        &Mutex::new(None),
        &Mutex::new(Vec::new()),
    );
    assert_eq!(lock(&result).unwrap().0, TerminalStatus::Exited);
    assert!(member.current().is_none_or(|row| row.zombie));
}

fn read_member_pid(worker: &mut Worker) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        worker.io_round(&mut VecDeque::new()).unwrap();
        if let Some(pid) = std::str::from_utf8(lock(&worker.output).bytes.make_contiguous())
            .unwrap()
            .split_once('\n')
            .and_then(|(line, _)| line.strip_prefix("MEMBER "))
        {
            return pid.trim_end().parse().unwrap();
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("the member did not report its signal dispositions"));
        let mut descriptors = [rustix::event::PollFd::new(
            worker.io.as_ref().unwrap(),
            rustix::event::PollFlags::IN,
        )];
        rustix::event::poll(
            &mut descriptors,
            Some(&rustix::event::Timespec {
                tv_sec: remaining.as_secs().try_into().unwrap(),
                tv_nsec: remaining.subsec_nanos().into(),
            }),
        )
        .unwrap();
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
        let (observed, receipt) = sync_channel(1);
        worker.escape_watch = Some(EscapeWatch {
            pid: escaped_pid,
            started: escape.0.started.clone(),
            observed,
        });
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
        // Close only after the worker has recorded the escape while its parent is still live.
        receipt
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|error| panic!("worker did not record the PTY escape: {error}"));
        closing.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(30);
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
    let ready = PathBuf::from(std::env::var_os("PLAK_OPERATOR_LOSS_HANDSHAKE").unwrap());
    let mut command = CommandBuilder::new("/bin/sh");
    command.env_clear();
    command.args([
        "-c",
        "trap '' HUP; printf 'ORPHAN-READY\n'; n=0; while [ ! -f \"$1\" ]; do \
         [ \"$n\" -lt 3000 ] || exit 1; n=$((n+1)); /bin/sleep 0.02; done",
        "orphaned-shell",
    ]);
    command.arg(ready.with_extension("release"));
    // A noninteractive shell does not return to a prompt on the hung-up PTY.
    let mut worker = Worker::spawn_command(
        command,
        TerminalDimensions::new(24, 80).unwrap(),
        Arc::default(),
    )
    .unwrap();
    until_output(&mut worker, b"ORPHAN-READY\r\n");
    let pending = ready.with_extension("pending");
    std::fs::write(&pending, worker.identity.as_ref().unwrap().pid.to_string()).unwrap();
    std::fs::rename(pending, ready).unwrap();
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
    // This fixture already knows the exact shell PID; it does not need a
    // bounded whole-machine scan to prove that this one identity still lives.
    let escape = Escape(Identity::capture_fixture_pid(pid).unwrap());
    runner.kill().unwrap();
    runner.wait().unwrap();
    assert!(
        escape.current().is_some_and(|row| !row.zombie),
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
    assert!(
        escape.current().is_some_and(|row| !row.zombie),
        "refusing a lost terminal must not kill the surviving shell"
    );
    std::fs::write(handshake.with_extension("release"), b"").unwrap();
}
