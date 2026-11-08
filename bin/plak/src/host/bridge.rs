//! The public bridge owns discovery, never the client's handshake or runtime custody.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::wire_transport::hosts;
use crate::{CliFailure, CliResult, ProcessOutcome, product_paths::PlakPaths};

const DISCOVERY_WINDOW: Duration = Duration::from_secs(10);

pub(super) fn connect(paths: &PlakPaths) -> CliResult<ProcessOutcome> {
    let deadline = Instant::now() + DISCOVERY_WINDOW;
    let key = crate::home::state::state_key();
    loop {
        let live = hosts::live_until(paths, deadline)?;
        if live.len() > 1 || live.iter().any(|host| host.key != key) {
            return Err(hosts::conflict(&live));
        }
        match hosts::socket_until(&paths.host_socket(), deadline) {
            Ok(stream) => return relay(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) => {}
            Err(error) => {
                return Err(CliFailure::new(format!(
                    "the host socket could not be verified: {error}"
                )));
            }
        }
        if live.is_empty() && hosts::lock_holder(&hosts::home_lock(paths))?.is_none() {
            return Err(CliFailure::new(
                "no running host for this home; start plak host or its user unit; no host or embedded session was started",
            ));
        }
        if Instant::now() >= deadline {
            return Err(CliFailure::new(
                "the host is starting or stopping; retry; no host or embedded session was started",
            ));
        }
        std::thread::sleep(
            Duration::from_millis(25).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn relay(mut stream: UnixStream) -> CliResult<ProcessOutcome> {
    let mut writer = stream.try_clone().map_err(failed)?;
    // Each direction has one bounded buffer. EOF closes the
    // socket's write half; the host detaches this client, not its session.
    let input = std::thread::spawn(move || {
        let result = forward(&mut io::stdin().lock(), &mut writer);
        let _ = writer.shutdown(Shutdown::Write);
        result
    });
    let result = forward(&mut stream, &mut io::stdout().lock());
    let _ = stream.shutdown(Shutdown::Both);
    result.map_err(failed)?;
    // If the host closed first, stdin may still be waiting for its owner.
    // Do not park process exit on that read; it owns no host/session work.
    if input.is_finished() {
        input
            .join()
            .map_err(|_| CliFailure::new("the bridge input reader failed"))?
            .map_err(failed)?;
    }
    Ok(ProcessOutcome::Success)
}

fn forward(reader: &mut impl Read, writer: &mut impl Write) -> io::Result<()> {
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        writer.write_all(&buffer[..count])?;
        // The client's next request may wait for this frame; never retain
        // it in stdout until EOF or a later frame fills the buffer.
        writer.flush()?;
    }
}

fn failed(error: io::Error) -> CliFailure {
    CliFailure::new(format!("host bridge disconnected: {error}"))
}
