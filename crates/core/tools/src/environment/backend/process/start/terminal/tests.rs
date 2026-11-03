use std::io::Read;

use super::*;
use crate::environment::backend::process::{JoinedSessionOutput, process_group_exists};
use crate::{JoinedProcessCommand, OperationContext};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn assert_stdin_still_open(mut input: std::io::PipeReader) {
    let mut bytes = [0; 4096];
    loop {
        match input.read(&mut bytes) {
            Ok(0) => panic!("stdin EOF preceded rollback group termination"),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("stdin probe failed: {error}"),
        }
    }
}

#[test]
fn pipes_initial_stdin_rollback_terminates_group_before_join() -> TestResult {
    let (input_reader, input_writer) = std::io::pipe()?;
    set_nonblocking(&input_reader)?;
    let child = Command::new("/bin/sh")
        .args(["-c", "trap '' TERM; printf r; exec sleep 30"])
        .stdin(Stdio::from(input_reader.try_clone()?))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let group =
        rustix::process::Pid::from_raw(i32::try_from(child.id())?).ok_or("invalid child pid")?;
    let mut resources = JoinedProcessResources {
        child: JoinedChild::Pipes(child, None),
        process_group: group,
        readers: Vec::new(),
        input_thread: None,
        input_owner: None,
        io_stop: Arc::new(AtomicBool::new(false)),
        cleanup_duration: Duration::from_secs(4),
        armed: true,
        before_rollback: Some(Box::new(move || assert_stdin_still_open(input_reader))),
        before_drain: None,
    };
    let JoinedChild::Pipes(child, _) = &mut resources.child else {
        return Err("expected pipes".into());
    };
    let stdin = std::process::ChildStdin::from(std::os::fd::OwnedFd::from(input_writer));
    set_nonblocking(&stdin)?;
    let mut stdout = child.stdout.take().ok_or("stdout")?;
    let stderr = child.stderr.take().ok_or("stderr")?;
    let mut ready = [0];
    stdout.read_exact(&mut ready)?;
    assert_eq!(ready, [b'r']);

    // This worker cannot join while the owned group is alive, unlike an
    // ordinary output reader that may stop as soon as io_stop is set.
    resources.readers.push(std::thread::spawn(move || {
        let started = Instant::now();
        let deadline = started + Duration::from_secs(10);
        while process_group_exists(group) && Instant::now() < deadline {
            sleep_until_next_poll(started, deadline);
        }
    }));
    let shared = Arc::new(Mutex::new(JoinedSessionShared {
        output: JoinedSessionOutput::default(),
        status: JoinedProcessStatus::Running,
        elapsed: None,
        terminal_changes: None,
        changes: tokio::sync::watch::channel(0).0,
        terminal: Arc::new(std::sync::Condvar::new()),
        released: false,
        spool: None,
        pending_spool: None,
        leftover: None,
        disowned: false,
    }));
    let failed = Arc::new(AtomicBool::new(false));
    let limits = ExecutionEnvironmentLimits::new(
        4096,
        4096,
        8192,
        4096,
        Duration::from_millis(50),
        Duration::from_secs(4),
    )?;
    let request = JoinedProcessStartRequest::new(
        JoinedProcessCommand::literal("/bin/sh", [String::from("-c"), String::from("unused")])?,
        ".",
        vec![b'x'; 1024 * 1024],
        false,
        JoinedProcessTerminalMode::Pipes,
    )?;
    let error = resources
        .start_io(
            JoinedIo::Pipes {
                stdin: Some(stdin),
                stdout,
                stderr,
            },
            &shared,
            &failed,
            limits,
            &request,
        )
        .err()
        .ok_or("initial input did not fail")?;
    assert_eq!(
        error.kind(),
        super::super::ExecutionBackendErrorKind::TimedOut
    );
    assert_eq!(
        error.message().as_str(),
        "joined process initial input timed out"
    );
    assert!(!process_group_exists(group));
    assert!(resources.readers.is_empty());
    Ok(())
}

struct DelayedPtyReader {
    file: std::fs::File,
    release: Option<std::sync::mpsc::Receiver<()>>,
    workspace: tempfile::TempDir,
}

impl std::os::fd::AsFd for DelayedPtyReader {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl Read for DelayedPtyReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let count = self.file.read(bytes)?;
        if count != 0
            && let Some(release) = self.release.take()
        {
            // Read before macOS revokes the PTY, but withhold the byte from
            // the output worker until the exited leader's cleanup drains it.
            std::fs::write(self.workspace.path().join("leader-release"), b"release")?;
            release.recv_timeout(Duration::from_secs(5)).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "PTY drain was not entered")
            })?;
        }
        Ok(count)
    }
}

fn delayed_pty() -> Result<(JoinedProcessResources, DelayedPtyReader), Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let pair = native_pty_system().openpty(PtySize::default())?;
    let file = clone_linux_pty_master(pair.master.as_ref())?;
    let mut command = portable_pty::CommandBuilder::new("/bin/sh");
    command.cwd(workspace.path());
    command.args([
        "-c",
        "printf x; while [ ! -f leader-release ]; do sleep 0.01; done",
    ]);
    let child = pair.slave.spawn_command(command)?;
    let process_group = child
        .process_id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
        .ok_or("missing PTY process group")?;
    drop(pair);
    let (release, released) = std::sync::mpsc::sync_channel(1);
    Ok((
        JoinedProcessResources {
            child: JoinedChild::Pty(child, None),
            process_group,
            readers: Vec::new(),
            input_thread: None,
            input_owner: None,
            io_stop: Arc::new(AtomicBool::new(false)),
            cleanup_duration: Duration::from_secs(4),
            armed: true,
            before_rollback: None,
            before_drain: Some(release),
        },
        DelayedPtyReader {
            file,
            release: Some(released),
            workspace,
        },
    ))
}

#[test]
fn pty_reader_drains_after_leader_exit_before_io_stop() -> TestResult {
    let (mut resources, reader) = delayed_pty()?;
    let shared = Arc::new(Mutex::new(JoinedSessionShared {
        output: JoinedSessionOutput::default(),
        status: JoinedProcessStatus::Running,
        elapsed: None,
        terminal_changes: None,
        changes: tokio::sync::watch::channel(0).0,
        terminal: Arc::new(std::sync::Condvar::new()),
        released: false,
        spool: None,
        pending_spool: None,
        leftover: None,
        disowned: false,
    }));
    let failed = Arc::new(AtomicBool::new(false));
    let limits = ExecutionEnvironmentLimits::new(
        4096,
        4096,
        8192,
        4096,
        Duration::from_secs(20),
        Duration::from_secs(4),
    )?;
    resources.add_reader(
        reader,
        JoinedProcessOutputStream::Terminal,
        &shared,
        &failed,
        limits,
    )?;
    // Hold delivery of the final physical byte until cleanup drains it. The
    // supervisor must reap the leader and collect that byte before stop.
    super::super::super::supervise_joined_process(
        &mut resources,
        limits,
        &OperationContext::new(plakous_model::CancellationToken::new()),
        &super::super::AtomicU8::new(super::super::FOREGROUND),
        &super::super::AtomicU64::new(200),
        &AtomicBool::new(false),
        &failed,
        &shared,
        false,
    );
    resources.disarm();
    let shared = shared.lock().map_err(|_| "poisoned output")?;
    let bytes: Vec<_> = shared
        .output
        .records
        .iter()
        .flat_map(|record| record.bytes.iter().copied())
        .collect();
    assert_eq!(bytes, b"x");
    assert!(shared.status.is_terminal());
    assert!(resources.readers.is_empty());
    Ok(())
}
