//! Same-PID transport handoff. The kernel keeps queued connections and
//! flock descriptions alive inside a private Unix socketpair message.

use std::io::{IoSlice, IoSliceMut};
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

const MAX_FDS: usize = 5;

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

pub(crate) fn receive() -> CliResult<Option<(String, Inherited)>> {
    let Some(envelope) = crate::environment::host::reexec() else {
        return Ok(None);
    };
    let envelope: Envelope = serde_json::from_str(&envelope).map_err(failed)?;
    if envelope.pid != std::process::id() {
        return Ok(None);
    }
    if !crate::carry::upgrade::binary_id_valid(&envelope.binary) {
        return Err(failed("invalid inherited binary identity"));
    }
    let stdin = std::io::stdin();
    let fds = receive_fds(stdin.as_fd())?;
    let mut fds = fds.into_iter();
    let mut next = || {
        fds.next()
            .ok_or_else(|| failed("missing inherited descriptor"))
    };
    // Restore stdin before startup can launch any child process.
    rustix::stdio::dup2_stdin(next()?).map_err(failed)?;
    let inherited = Inherited {
        home_lock: next()?,
        state_lock: next()?,
        control: next()?,
        web: envelope.web.then(&mut next).transpose()?,
        environment: envelope.environment,
    };
    if fds.next().is_some() {
        return Err(failed("unexpected inherited descriptor"));
    }
    Ok(Some((envelope.binary, inherited)))
}

pub(super) fn replace(
    ready: super::upgrade::Ready,
    home: &std::path::Path,
    arguments: &[String],
    environment: &HostEnvironment,
    ownership: &super::HostOwnership,
    listeners: &super::HostListeners,
) -> CliResult<crate::ProcessOutcome> {
    let destination = ready.paired_command(home, arguments);
    let target = ready.binary.clone();
    // Release store and session holds before the successor starts.
    drop(ready);
    let descriptors = [
        ownership.home.as_fd(),
        ownership.state.as_fd(),
        listeners.control.as_fd(),
    ];
    let web = listeners.web.as_deref().map(crate::web::Listener::as_fd);
    let failure = match destination {
        Ok(command) => exec(command, &target, environment, descriptors, web),
        Err(failure) => failure,
    };
    eprintln!("{failure}; restoring the old host");
    let source = std::env::current_exe().map_err(failed)?;
    let mut command =
        crate::carry::upgrade::process::command(&source, home, &crate::home::state::state_key());
    command.args(arguments);
    Err(exec(
        command,
        crate::build_identity::binary_id(),
        environment,
        descriptors,
        web,
    ))
}

fn exec(
    mut command: Command,
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
        let (send, receive) = UnixDatagram::pair().map_err(failed)?;
        let fds = [saved_stdin.as_fd(), home_lock, state_lock, control];
        let mut fds = fds.to_vec();
        fds.extend(web);
        send_fds(&send, &fds)?;
        let envelope = serde_json::to_string(&Envelope {
            pid: std::process::id(),
            binary: binary.to_owned(),
            environment: environment.clone(),
            web: web.is_some(),
        })
        .map_err(failed)?;
        // Command owns the only exception to close-on-exec: this one endpoint
        // temporarily occupies stdin. The listening sockets are never made
        // inheritable in the source's descriptor table.
        let error = command
            .env(crate::environment::HOST_REEXEC, envelope)
            .stdin(Stdio::from(OwnedFd::from(receive)))
            .exec();
        rustix::stdio::dup2_stdin(saved_stdin).map_err(failed)?;
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
