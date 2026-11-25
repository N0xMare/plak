//! Bounded subprocess capture for the private install and deployment commands.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek as _};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::CliResult;
use crate::shipping_artifact::{check, fail, io};

pub(crate) fn command(program: &OsStr) -> Command {
    Command::new(program)
}

pub(crate) struct Output {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

pub(crate) fn capture(
    command: Command,
    input: Option<File>,
    timeout: Duration,
    limit: u64,
) -> CliResult<Output> {
    capture_bounded(command, input, timeout, limit, 4096)
}

pub(crate) fn capture_with_stderr(
    command: Command,
    input: Option<File>,
    timeout: Duration,
    limit: u64,
) -> CliResult<Output> {
    capture_bounded(command, input, timeout, limit, 64 * 1024)
}

fn capture_bounded(
    mut command: Command,
    input: Option<File>,
    timeout: Duration,
    limit: u64,
    stderr_limit: u64,
) -> CliResult<Output> {
    let mut output = tempfile::tempfile().map_err(io)?;
    let mut errors = tempfile::tempfile().map_err(io)?;
    let status = run(
        command
            .stdin(input.map_or_else(Stdio::null, Stdio::from))
            .stdout(output.try_clone().map_err(io)?)
            .stderr(errors.try_clone().map_err(io)?),
        timeout,
        || {
            check(
                output.metadata().map_err(io)?.len() <= limit
                    && errors.metadata().map_err(io)?.len() <= 64 * 1024,
                "command exceeded its output bound",
            )
        },
    )?;
    output.rewind().map_err(io)?;
    errors.rewind().map_err(io)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    output
        .take(limit + 1)
        .read_to_end(&mut stdout)
        .map_err(io)?;
    errors
        .take(stderr_limit)
        .read_to_end(&mut stderr)
        .map_err(io)?;
    check(
        stdout.len() as u64 <= limit,
        "command exceeded its output bound",
    )?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

pub(crate) fn run(
    command: &mut Command,
    timeout: Duration,
    check_output: impl Fn() -> CliResult<()>,
) -> CliResult<ExitStatus> {
    let mut child = command.process_group(0).spawn().map_err(io)?;
    let stop = Instant::now() + timeout;
    loop {
        let verdict = check_output().and_then(|()| child.try_wait().map_err(io));
        let error = match verdict {
            Ok(Some(status)) => {
                check_output()?;
                return Ok(status);
            }
            Ok(None) if Instant::now() < stop => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Ok(None) => fail("command exceeded its deadline"),
            Err(error) => error,
        };
        if let Ok(id) = i32::try_from(child.id())
            && let Some(group) = rustix::process::Pid::from_raw(id)
        {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
}
