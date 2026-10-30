use std::io::Read;

use super::*;
use crate::JoinedProcessCommand;
use crate::environment::backend::process::{JoinedSessionOutput, process_group_exists};

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
