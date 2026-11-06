//! The local host's socket as a wire v2 client transport
//! ([`plakous_host_protocol::client`]; a child `plak agent --wire v2`'s
//! stdio is [`Transport::new`] over its pipes). The socket (spec 04 §4.7)
//! is a same-user channel on the client's side too: it is used only in a
//! private directory of the client's own user, and only when the socket is
//! that user's.

use std::io;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::os::unix::net::UnixStream;
use std::path::Path;

use plakous_host_protocol::client::Transport;

/// The protocol extension Plak's hosts speak: what a wire client of one
/// is typed with.
pub use crate::surface_protocol::PlakExtension;

/// Connects to the host socket at `path` after checking that the socket
/// and its directory belong to this user, the directory is closed to
/// everyone else, and the file is a socket; once connected, the peer must
/// be this user's process.
///
/// # Errors
///
/// [`io::ErrorKind::PermissionDenied`] naming what is not private, or the
/// failed connection.
pub fn host_socket(path: &Path) -> io::Result<Transport> {
    let refused = |what: String| io::Error::new(io::ErrorKind::PermissionDenied, what);
    let user = rustix::process::geteuid().as_raw();
    let directory = path
        .parent()
        .ok_or_else(|| refused(format!("{} has no directory", path.display())))?;
    let run = std::fs::symlink_metadata(directory)?;
    if !run.is_dir() || run.uid() != user || run.mode() & 0o077 != 0 {
        return Err(refused(format!(
            "{} is not a directory private to this user",
            directory.display()
        )));
    }
    let socket = std::fs::symlink_metadata(path)?;
    if !socket.file_type().is_socket() || socket.uid() != user {
        return Err(refused(format!(
            "{} is not this user's socket",
            path.display()
        )));
    }
    let stream = UnixStream::connect(path)?;
    // The path checks name what is wrong; the peer's credentials decide,
    // since a directory above the socket's may be another user's to swap.
    peer_is(peer_uid(&stream), user)?;
    let interrupt = stream.try_clone()?;
    Ok(
        Transport::new(stream.try_clone()?, stream).with_welcome_deadline(
            std::time::Duration::from_secs(10),
            move || {
                let _ = interrupt.shutdown(std::net::Shutdown::Both);
            },
        ),
    )
}

/// The uid of `stream`'s peer (`SO_PEERCRED`, or `getpeereid` on macOS).
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let probe = stream.try_clone()?;
    probe.set_nonblocking(true)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?;
    let uid = runtime.block_on(async {
        let probe = tokio::net::UnixStream::from_std(probe)?;
        probe.peer_cred().map(|peer| peer.uid())
    });
    // The probe shared the stream's open file; the client reads it blocking.
    stream.set_nonblocking(false)?;
    uid
}

/// Refuses a peer that is not `user`, or whose credentials could not be
/// read.
fn peer_is(peer: io::Result<u32>, user: u32) -> io::Result<()> {
    match peer {
        Ok(peer) if peer == user => Ok(()),
        Ok(peer) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the host socket's peer is uid {peer}, not this user's {user}"),
        )),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the host socket's peer could not be read: {error}"),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
