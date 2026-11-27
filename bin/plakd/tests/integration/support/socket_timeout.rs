use std::io;

pub(super) fn allow_closed_peer(result: io::Result<()>) -> io::Result<()> {
    // macOS can reject a timeout on a Unix socket after the peer closes.
    // Let the next I/O observe the close, including any buffered data.
    #[cfg(target_os = "macos")]
    if let Err(error) = &result
        && error.raw_os_error() == Some(rustix::io::Errno::INVAL.raw_os_error())
    {
        return Ok(());
    }
    result
}
