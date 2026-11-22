//! Same-PID transport handoff. The kernel keeps queued connections and
//! flock descriptions alive inside a private Unix socketpair message.

use std::io::{IoSlice, IoSliceMut, Seek as _};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::os::unix::{net::UnixDatagram, process::CommandExt as _};
use std::process::{Command, Stdio};

use rustix::io::{FdFlags, fcntl_setfd};
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use serde::{Deserialize, Serialize};

use crate::commands::runner::environment::HostEnvironment;
use crate::{CliFailure, CliResult};

const MAX_FDS: usize = 6;
const MAX_ENVELOPE_BYTES: u64 = 1024 * 1024;

#[derive(Deserialize, Serialize)]
struct Envelope {
    pid: u32,
    binary: String,
    environment: HostEnvironment,
    web: bool,
}

pub(crate) struct Inherited {
    pub(super) home_lock: OwnedFd,
    pub(super) state_lock: OwnedFd,
    pub(super) control: OwnedFd,
    pub(super) web: Option<OwnedFd>,
    pub(super) environment: HostEnvironment,
}

fn failed(error: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(format!("host upgrade transport handoff: {error}"))
}

fn restore_stdio(
    input: BorrowedFd<'_>,
    output: BorrowedFd<'_>,
    error: BorrowedFd<'_>,
) -> CliResult<()> {
    use rustix::stdio::{dup2_stderr, dup2_stdin, dup2_stdout};
    let input = dup2_stdin(input);
    let output = dup2_stdout(output);
    let error = dup2_stderr(error);
    input.and(output).and(error).map_err(failed)
}

pub(crate) fn receive() -> CliResult<(String, Inherited)> {
    let stdin = std::io::stdin();
    let fds = receive_fds(stdin.as_fd())?;
    let mut fds = fds.into_iter();
    let mut next = || {
        fds.next()
            .ok_or_else(|| failed("missing inherited descriptor"))
    };
    let control = std::io::stdout().as_fd().try_clone_to_owned();
    let web = std::io::stderr().as_fd().try_clone_to_owned();
    let [input, output, error] = [next()?, next()?, next()?];
    restore_stdio(input.as_fd(), output.as_fd(), error.as_fd())?;
    let control = control.map_err(failed)?;
    let file = std::fs::File::from(next()?);
    let metadata = file.metadata().map_err(failed)?;
    if !metadata.is_file() || metadata.len() > MAX_ENVELOPE_BYTES {
        return Err(failed("invalid inherited envelope file"));
    }
    let envelope: Envelope =
        serde_json::from_reader(std::io::BufReader::new(file)).map_err(failed)?;
    if envelope.pid != std::process::id()
        || !crate::carry::upgrade::binary_id_valid(&envelope.binary)
    {
        return Err(failed("invalid inherited host identity"));
    }
    let inherited = Inherited {
        home_lock: next()?,
        state_lock: next()?,
        control,
        web: envelope.web.then_some(web).transpose().map_err(failed)?,
        environment: envelope.environment,
    };
    if fds.next().is_some() {
        return Err(failed("unexpected inherited descriptor"));
    }
    Ok((envelope.binary, inherited))
}

pub(super) fn replace(
    ready: super::upgrade::Ready,
    home: &std::path::Path,
    arguments: &[String],
    environment: &HostEnvironment,
    ownership: &super::HostOwnership,
    listeners: super::HostListeners,
) -> CliResult<crate::ProcessOutcome> {
    let listeners = ready.inherits_transport().then_some(listeners);
    crate::test_hooks::host_lifecycle::before_paired_exec();
    let destination = ready.paired_command(home);
    let target = ready.binary.clone();
    // Release store and session holds before the successor starts.
    drop(ready);
    let replace = |mut command: Command, binary: &str| {
        let Some(listeners) = &listeners else {
            // The listeners are closed. The source still holds both locks;
            // exec closes them so a legacy successor can take them normally.
            return failed(command.args(arguments).exec());
        };
        let descriptors = [
            ownership.home.as_fd(),
            ownership.state.as_fd(),
            listeners.control.as_fd(),
        ];
        let web = listeners.web.as_deref().map(crate::web::Listener::as_fd);
        exec(
            command,
            home,
            arguments,
            binary,
            environment,
            descriptors,
            web,
        )
    };
    let failure = match destination {
        Ok(command) => {
            crate::carry::upgrade::crash_cut("after-source-release");
            replace(command, &target)
        }
        Err(failure) => failure,
    };
    eprintln!("{failure}; restoring the old host");
    let source = std::env::current_exe().map_err(failed)?;
    let command =
        crate::carry::upgrade::process::command(&source, home, &crate::home::state::state_key());
    Err(replace(command, crate::build_identity::binary_id()))
}

fn exec(
    mut command: Command,
    home: &std::path::Path,
    arguments: &[String],
    binary: &str,
    environment: &HostEnvironment,
    [home_lock, state_lock, control]: [BorrowedFd<'_>; 3],
    web: Option<BorrowedFd<'_>>,
) -> CliFailure {
    let result = (|| -> CliResult<CliFailure> {
        let saved_stdin = std::io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .map_err(failed)?;
        let saved_stdout = std::io::stdout()
            .as_fd()
            .try_clone_to_owned()
            .map_err(failed)?;
        let saved_stderr = std::io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .map_err(failed)?;
        let (send, receive) = UnixDatagram::pair().map_err(failed)?;
        // Pass metadata by descriptor as well: no private marker or PATH/HOME
        // can leak through the successor's process environment to its tools.
        let mut file = tempfile::tempfile_in(home).map_err(failed)?;
        serde_json::to_writer(
            &mut file,
            &Envelope {
                pid: std::process::id(),
                binary: binary.to_owned(),
                environment: environment.clone(),
                web: web.is_some(),
            },
        )
        .map_err(failed)?;
        if file.stream_position().map_err(failed)? > MAX_ENVELOPE_BYTES {
            return Err(failed("handoff envelope exceeds its size limit"));
        }
        file.rewind().map_err(failed)?;
        let fds = [
            saved_stdin.as_fd(),
            saved_stdout.as_fd(),
            saved_stderr.as_fd(),
            file.as_fd(),
            home_lock,
            state_lock,
        ];
        send_fds(&send, &fds)?;
        // A Darwin Unix listener received by SCM_RIGHTS can make accepted
        // streams report false EOF. Pass listeners directly through stdio;
        // their original descriptors remain CLOEXEC.
        let control = control.try_clone_to_owned().map_err(failed)?;
        let web = web
            .map(|fd| fd.try_clone_to_owned())
            .transpose()
            .map_err(failed)?;
        command
            .arg(crate::HOST_REEXEC_ARG)
            .args(arguments)
            .stdin(Stdio::from(OwnedFd::from(receive)))
            .stdout(Stdio::from(control));
        if let Some(web) = web {
            command.stderr(Stdio::from(web));
        }
        let error = command.exec();
        restore_stdio(
            saved_stdin.as_fd(),
            saved_stdout.as_fd(),
            saved_stderr.as_fd(),
        )?;
        Ok(failed(error))
    })();
    match result {
        Ok(failure) | Err(failure) => failure,
    }
}

fn send_fds(send: &UnixDatagram, fds: &[BorrowedFd<'_>]) -> CliResult<()> {
    let mut bytes = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut bytes);
    if !control.push(SendAncillaryMessage::ScmRights(fds)) {
        return Err(failed("too many descriptors"));
    }
    sendmsg(
        send,
        &[IoSlice::new(b"P")],
        &mut control,
        SendFlags::empty(),
    )
    .map_err(failed)?;
    Ok(())
}

fn receive_fds(receive: BorrowedFd<'_>) -> CliResult<Vec<OwnedFd>> {
    let mut bytes = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut bytes);
    let mut data = [0];
    let received = recvmsg(
        receive,
        &mut [IoSliceMut::new(&mut data)],
        &mut control,
        RecvFlags::DONTWAIT,
    )
    .map_err(failed)?;
    if received.bytes != 1
        || data != *b"P"
        || received
            .flags
            .intersects(ReturnFlags::CTRUNC | ReturnFlags::TRUNC)
    {
        return Err(failed("invalid descriptor packet"));
    }
    let mut fds = Vec::new();
    for message in control.drain() {
        if let RecvAncillaryMessage::ScmRights(received) = message {
            for fd in received {
                // macOS has no MSG_CMSG_CLOEXEC. Startup has not launched children.
                fcntl_setfd(&fd, FdFlags::CLOEXEC).map_err(failed)?;
                fds.push(fd);
            }
        }
    }
    Ok(fds)
}

pub(crate) fn listener(fd: &OwnedFd) -> std::io::Result<()> {
    // Darwin does not implement SO_ACCEPTCONN. Both selectors also check
    // the configured local address when they adopt the stream socket.
    let valid = rustix::net::sockopt::socket_type(fd)? == rustix::net::SocketType::STREAM;
    #[cfg(target_os = "linux")]
    let valid = valid && rustix::net::sockopt::socket_acceptconn(fd)?;
    if !valid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a listening socket",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    #[test]
    fn a_tcp_connect_waits_in_the_backlog_between_owners() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (send, receive) = UnixDatagram::pair().unwrap();
        send_fds(&send, &[listener.as_fd()]).unwrap();
        drop(listener);
        // Nothing in userspace owns the listener while the rights are in flight.
        let _client = std::net::TcpStream::connect(address).unwrap();
        let mut fds = receive_fds(receive.as_fd()).unwrap();
        assert_eq!(fds.len(), 1);
        let fd = fds.pop().unwrap();
        assert!(
            rustix::io::fcntl_getfd(&fd)
                .unwrap()
                .contains(FdFlags::CLOEXEC)
        );
        let adopted = std::net::TcpListener::from(fd);
        adopted.set_nonblocking(true).unwrap();
        assert!(adopted.accept().is_ok());
    }
}
